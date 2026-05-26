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
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Semaphore};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tracing::{debug, info, trace, warn};

use protocol_types::messages::{
    BucketUpdatePayload, ErrorPayload, HelloAckPayload, RetailHelloPayload, RetailToService,
    RfqResponsePayload, ServiceToRetail,
};

use crate::{rfq, AppState, Config};

pub async fn handle(
    ws: WebSocketStream<TcpStream>,
    _peer: SocketAddr,
    state: Arc<AppState>,
    cfg: Arc<Config>,
    _hello: RetailHelloPayload,
) -> Result<()> {
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
        match msg {
            RetailToService::SubscribeBuckets { payload } => {
                debug!(buckets = payload.bucket_ids.len(), "retail subscribe buckets");
                for id in payload.bucket_ids {
                    if let Some(b) = state.buckets.get(&id) {
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
                        payload.bucket_id,
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
