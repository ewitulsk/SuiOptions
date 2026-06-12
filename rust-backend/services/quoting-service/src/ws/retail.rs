//! Retail (writer or trader) handler.
//!
//! No auth — retail is anonymous over the WS; they sign and pay for the
//! eventual on-chain PTB themselves.
//!
//! Read loop dispatches:
//!
//! - `Hello` → already consumed by [`super::dispatch`]; ignore if it shows
//!   up again.
//! - `SubscribeBuckets` → record the bucket ids; for MVP we reply with the
//!   current state of each subscribed bucket and trust the indexer's
//!   downstream events to push updates (full BucketUpdate fan-out wiring
//!   lives in the indexer client).
//! - `RFQRequest` → call [`crate::rfq::orchestrate`] with the request_id
//!   and ship the result as `RFQResponse`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Semaphore};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tracing::{debug, info, warn, Instrument};

use protocol_types::messages::{
    BucketUpdatePayload, BulkViewRfqResponsePayload, ErrorPayload, HelloAckPayload,
    RetailHelloPayload, RetailToService, RfqResponsePayload, ServiceToRetail,
};

use crate::state::RfqObservation;
use crate::{rfq, AppState, Config};

pub async fn handle(
    ws: WebSocketStream<TcpStream>,
    _peer: SocketAddr,
    state: Arc<AppState>,
    cfg: Arc<Config>,
    _hello: RetailHelloPayload,
) -> Result<()> {
    let _conn_gauge = super::ConnGauge::new("retail");
    let (mut sink, mut stream) = ws.split();
    let session_id = uuid::Uuid::new_v4().to_string();
    sink.send(Message::Text(serde_json::to_string(&ServiceToRetail::HelloAck {
        payload: HelloAckPayload {
            session_id: session_id.clone(),
        },
    })?))
    .await?;
    info!(session_id, "retail hello-acked");

    // Per-session inflight cap on RFQ orchestrations. Combined with the
    // global cap on AppState, this prevents one client (or the storm we
    // saw in SO-65) from spawning unbounded tokio tasks.
    let session_inflight = Arc::new(Semaphore::new(cfg.max_inflight_rfqs_per_session));

    let (out_tx, mut out_rx) = mpsc::channel::<ServiceToRetail>(64);
    let write_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let text = match serde_json::to_string(&msg) {
                Ok(t) => t,
                Err(e) => {
                    warn!(error = %e, "encode retail frame");
                    continue;
                }
            };
            metrics::counter!("quoting_ws_messages_total", "role" => "retail", "direction" => "out", "type" => out_type(&msg)).increment(1);
            if let Err(e) = sink.send(Message::Text(text)).await {
                debug!(error = %e, "retail sink closed");
                break;
            }
        }
    });

    while let Some(frame) = stream.next().await {
        let frame = match frame {
            Ok(f) => f,
            Err(e) => {
                debug!(error = %e, "retail read err");
                break;
            }
        };
        let text = match frame {
            Message::Text(t) => t,
            Message::Binary(b) => String::from_utf8(b)?,
            Message::Close(_) => break,
            _ => continue,
        };
        let msg: RetailToService = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "bad retail frame");
                continue;
            }
        };
        metrics::counter!("quoting_ws_messages_total", "role" => "retail", "direction" => "in", "type" => in_type(&msg)).increment(1);
        match msg {
            RetailToService::SubscribeBuckets { payload } => {
                debug!(buckets = payload.bucket_ids.len(), "retail subscribe buckets");
                for id in payload.bucket_ids {
                    if let Ok(Some(b)) = state.indexer.bucket(id).await {
                        let _ = out_tx
                            .send(ServiceToRetail::BucketUpdate {
                                payload: BucketUpdatePayload {
                                    bucket_id: id,
                                    total_written: b.total_written,
                                    exercise_cursor: b.exercise_cursor,
                                    expiry_ms: b.expiry_ms,
                                },
                            })
                            .await;
                    }
                }
            }
            RetailToService::RFQRequest { request_id, payload } => {
                info!(
                    request_id = %request_id,
                    ?payload.side,
                    %payload.bucket_id,
                    write_amount = payload.write_amount,
                    "retail rfq request"
                );
                // Root span for the whole RFQ round trip; the orchestration
                // task below runs inside it so quote validation etc. show up
                // as child spans in Tempo. `trace_id` is only assigned once
                // the span is entered (otel layer), hence the record-after.
                let rfq_span = tracing::info_span!(
                    "rfq",
                    request_id = %request_id,
                    bucket_id = %payload.bucket_id,
                    trace_id = tracing::field::Empty,
                );
                let trace_id = {
                    let _enter = rfq_span.enter();
                    observability::current_trace_id()
                };
                if let Some(id) = &trace_id {
                    rfq_span.record("trace_id", id.as_str());
                }
                let rfq_started = Instant::now();
                // Best-effort observer fan-out (rfq-monitor and friends).
                // Fire before the rate-limit checks so operators see the
                // actual incoming traffic, including requests we then
                // reject. send() errors only when there are zero
                // subscribers, which is fine — drop the result.
                let _ = state.rfq_observers.send(RfqObservation {
                    timestamp_ms: now_ms(),
                    request_id: request_id.clone(),
                    bucket_id: payload.bucket_id,
                    write_amount: payload.write_amount,
                    side: payload.side,
                });
                // Resolve the bucket JIT. This both feeds the orchestrator
                // (strike/expiry for the broadcast) and lets us short-circuit
                // invalidated/unknown buckets with an explicit error instead
                // of a generic "no quotes" timeout. See SO-69.
                let bucket = match state.indexer.bucket(payload.bucket_id).await {
                    Ok(Some(b)) => b,
                    Ok(None) => {
                        debug!(request_id = %request_id, %payload.bucket_id, "rfq for unknown bucket");
                        let _ = out_tx
                            .send(ServiceToRetail::Error {
                                request_id: Some(request_id),
                                payload: ErrorPayload {
                                    code: "unknown_bucket".into(),
                                    message: "bucket is not known to the indexer".into(),
                                },
                            })
                            .await;
                        continue;
                    }
                    Err(e) => {
                        warn!(request_id = %request_id, %payload.bucket_id, error = %e, "indexer bucket lookup failed");
                        let _ = out_tx
                            .send(ServiceToRetail::Error {
                                request_id: Some(request_id),
                                payload: ErrorPayload {
                                    code: "indexer_unavailable".into(),
                                    message: "could not reach the indexer to price this request"
                                        .into(),
                                },
                            })
                            .await;
                        continue;
                    }
                };
                if bucket.invalidated {
                    debug!(request_id = %request_id, %payload.bucket_id, "rfq for invalidated bucket");
                    let _ = out_tx
                        .send(ServiceToRetail::Error {
                            request_id: Some(request_id),
                            payload: ErrorPayload {
                                code: "bucket_invalidated".into(),
                                message: "bucket has been invalidated by the protocol admin"
                                    .into(),
                            },
                        })
                        .await;
                    continue;
                }
                // Try to claim a per-session and global permit before
                // spawning. Both use `try_acquire_owned` (non-blocking) so
                // a saturated quota fails fast with a `rate_limited` error
                // rather than queuing arbitrary work.
                let session_permit = match Arc::clone(&session_inflight).try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        warn!(session_id, request_id = %request_id, "session rfq inflight cap hit");
                        let _ = out_tx
                            .send(ServiceToRetail::Error {
                                request_id: Some(request_id),
                                payload: ErrorPayload {
                                    code: "rate_limited".into(),
                                    message: "too many in-flight RFQs for this session"
                                        .into(),
                                },
                            })
                            .await;
                        continue;
                    }
                };
                let global_permit = match Arc::clone(&state.rfq_global_inflight)
                    .try_acquire_owned()
                {
                    Ok(p) => p,
                    Err(_) => {
                        warn!(request_id = %request_id, "global rfq inflight cap hit");
                        let _ = out_tx
                            .send(ServiceToRetail::Error {
                                request_id: Some(request_id),
                                payload: ErrorPayload {
                                    code: "rate_limited".into(),
                                    message: "service is at global RFQ capacity".into(),
                                },
                            })
                            .await;
                        // session_permit drops here, freeing its slot.
                        continue;
                    }
                };
                // Spawn the RFQ so the retail read loop isn't blocked by the
                // RFQ window — they may send more requests in the meantime.
                let state = Arc::clone(&state);
                let cfg = Arc::clone(&cfg);
                let out_tx = out_tx.clone();
                tokio::spawn(async move {
                    // Permits free on drop when the task exits — covers
                    // success, error, and panic paths.
                    let _session_permit = session_permit;
                    let _global_permit = global_permit;
                    let now = now_ms();
                    let quotes = rfq::orchestrate(
                        Arc::clone(&state),
                        payload.side,
                        bucket,
                        payload.write_amount,
                        request_id.clone(),
                        cfg.rfq_window,
                        cfg.protocol_id.clone(),
                        now,
                    )
                    .await;

                    if quotes.is_empty() {
                        let _ = out_tx
                            .send(ServiceToRetail::Error {
                                request_id: Some(request_id),
                                payload: ErrorPayload {
                                    code: "no_quotes".into(),
                                    message: "no MMs returned a valid quote within the window"
                                        .into(),
                                },
                            })
                            .await;
                        return;
                    }
                    let _ = out_tx
                        .send(ServiceToRetail::RFQResponse {
                            request_id,
                            payload: RfqResponsePayload {
                                bucket_id: payload.bucket_id,
                                write_amount: payload.write_amount,
                                quotes,
                            },
                        })
                        .await;
                    metrics::histogram!("quoting_rfq_roundtrip_duration_seconds")
                        .record(rfq_started.elapsed().as_secs_f64());
                }.instrument(rfq_span));
            }
            RetailToService::BulkViewRFQRequest { request_id, payload } => {
                debug!(
                    request_id = %request_id,
                    ?payload.side,
                    buckets = payload.bucket_ids.len(),
                    write_amount = payload.write_amount,
                    "retail bulk-view rfq request"
                );
                // Resolve each bucket JIT, dropping unknown/invalidated ones —
                // a quote against those would never execute, so they shouldn't
                // show an indicative premium either.
                let mut buckets = Vec::with_capacity(payload.bucket_ids.len());
                for id in &payload.bucket_ids {
                    match state.indexer.bucket(*id).await {
                        Ok(Some(b)) if !b.invalidated => buckets.push(b),
                        Ok(_) => {}
                        Err(e) => {
                            debug!(request_id = %request_id, bucket = %id, error = %e, "bulk-view bucket lookup failed; skipping");
                        }
                    }
                }
                // Share the same inflight caps as signed RFQs. On saturation
                // we just skip the bulk view (tiles keep their placeholder)
                // rather than erroring — it's a non-critical display path.
                let session_permit = match Arc::clone(&session_inflight).try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        debug!(session_id, request_id = %request_id, "session inflight cap hit; skipping bulk view");
                        continue;
                    }
                };
                let global_permit = match Arc::clone(&state.rfq_global_inflight).try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        debug!(request_id = %request_id, "global inflight cap hit; skipping bulk view");
                        continue;
                    }
                };
                let state = Arc::clone(&state);
                let cfg = Arc::clone(&cfg);
                let out_tx = out_tx.clone();
                tokio::spawn(async move {
                    let _session_permit = session_permit;
                    let _global_permit = global_permit;
                    let premiums = rfq::bulk_view::orchestrate_bulk_view(
                        Arc::clone(&state),
                        payload.side,
                        buckets,
                        payload.write_amount,
                        cfg.rfq_window,
                        cfg.bulk_view_cache_ttl,
                        now_ms(),
                    )
                    .await;
                    let _ = out_tx
                        .send(ServiceToRetail::BulkViewRFQResponse {
                            request_id,
                            payload: BulkViewRfqResponsePayload {
                                write_amount: payload.write_amount,
                                premiums,
                            },
                        })
                        .await;
                });
            }
            RetailToService::Hello { .. } => {
                debug!("ignored repeat hello");
            }
            RetailToService::Pong => {}
        }
    }

    write_task.abort();
    Ok(())
}

/// Variant name for the `type` label on `quoting_ws_messages_total`.
fn in_type(m: &RetailToService) -> &'static str {
    match m {
        RetailToService::Hello { .. } => "Hello",
        RetailToService::SubscribeBuckets { .. } => "SubscribeBuckets",
        RetailToService::RFQRequest { .. } => "RFQRequest",
        RetailToService::BulkViewRFQRequest { .. } => "BulkViewRFQRequest",
        RetailToService::Pong => "Pong",
    }
}

fn out_type(m: &ServiceToRetail) -> &'static str {
    match m {
        ServiceToRetail::HelloAck { .. } => "HelloAck",
        ServiceToRetail::BucketUpdate { .. } => "BucketUpdate",
        ServiceToRetail::RFQResponse { .. } => "RFQResponse",
        ServiceToRetail::BulkViewRFQResponse { .. } => "BulkViewRFQResponse",
        ServiceToRetail::Error { .. } => "Error",
        ServiceToRetail::Ping => "Ping",
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
