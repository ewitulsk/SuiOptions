//! Generic indexer WS subscriber.
//!
//! Connects to the indexer's WS fanout, sends one `Subscribe { after_sequence }`
//! at startup, then pumps `Snapshot` / `Event` frames into any type that
//! implements [`EventSink`]. Reconnects forever on disconnect — the indexer's
//! snapshot catch-up handles missed sequences during downtime.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use protocol_types::events::IndexedEvent;
use protocol_types::messages::{IndexerStream, IndexerSubscribe};

/// Trait implemented by any service's state that can ingest indexer events.
pub trait EventSink: Send + Sync + 'static {
    fn ingest_event(&self, event: &IndexedEvent);
}

/// Connect to `url`, then drive events into `sink` forever. Returns only on
/// fatal config error (which currently can't happen — bad URLs become
/// connect errors and we retry).
pub async fn run<S: EventSink>(url: String, sink: Arc<S>) -> Result<()> {
    let mut after = 0u64;
    loop {
        match tokio_tungstenite::connect_async(&url).await {
            Ok((mut ws, _)) => {
                info!(%url, after_sequence = after, "indexer subscriber connected");
                let sub = IndexerSubscribe::Subscribe {
                    after_sequence: after,
                };
                if let Err(e) = ws.send(Message::Text(serde_json::to_string(&sub)?)).await {
                    warn!(error = %e, "indexer subscribe send failed; reconnecting");
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
                while let Some(frame) = ws.next().await {
                    let frame = match frame {
                        Ok(f) => f,
                        Err(e) => {
                            warn!(error = %e, "indexer ws read failed; reconnecting");
                            break;
                        }
                    };
                    let text = match frame {
                        Message::Text(t) => t,
                        Message::Binary(b) => match String::from_utf8(b) {
                            Ok(s) => s,
                            Err(_) => continue,
                        },
                        Message::Close(_) => break,
                        _ => continue,
                    };
                    match serde_json::from_str::<IndexerStream>(&text) {
                        Ok(IndexerStream::Snapshot { payload }) => {
                            debug!(
                                events = payload.events.len(),
                                latest_sequence = payload.latest_sequence,
                                "indexer snapshot"
                            );
                            for e in &payload.events {
                                apply(&*sink, e, &mut after);
                            }
                            after = after.max(payload.latest_sequence);
                        }
                        Ok(IndexerStream::Event { payload }) => {
                            apply(&*sink, &payload, &mut after);
                        }
                        Ok(IndexerStream::Heartbeat { latest_sequence }) => {
                            debug!(latest_sequence, "indexer heartbeat");
                        }
                        Err(e) => {
                            warn!(error = %e, frame = %text, "bad indexer frame");
                        }
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, %url, "indexer connect failed");
            }
        }
        sleep(Duration::from_secs(1)).await;
    }
}

fn apply<S: EventSink>(sink: &S, ev: &IndexedEvent, after: &mut u64) {
    sink.ingest_event(ev);
    *after = (*after).max(ev.sequence);
}
