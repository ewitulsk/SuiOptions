//! Indexer subscriber.
//!
//! Connects to the indexer's WS, sends one `Subscribe { after_sequence: 0 }`
//! at startup, then pumps `Snapshot` / `Event` frames into the shared
//! [`AppState`]. Reconnects forever on disconnect — the indexer's
//! `Snapshot` catch-up handles missed sequences during downtime.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use protocol_types::events::IndexedEvent;
use protocol_types::messages::{IndexerStream, IndexerSubscribe};

use crate::state::AppState;

/// Connect to `url`, then drive events into `state` forever. Returns only on
/// fatal config error (which currently can't happen — bad URLs become
/// connect errors and we retry).
pub async fn run(url: String, state: Arc<AppState>) -> Result<()> {
    let mut after = 0u64;
    loop {
        match tokio_tungstenite::connect_async(&url).await {
            Ok((mut ws, _)) => {
                info!(%url, after_sequence = after, "indexer subscriber connected");
                let sub = IndexerSubscribe::Subscribe { after_sequence: after };
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
                            debug!(events = payload.events.len(), "snapshot");
                            for e in &payload.events {
                                apply(&state, e, &mut after);
                            }
                            // If the snapshot's `latest_sequence` is ahead of
                            // what we observed, jump forward so heartbeats
                            // don't look like gaps.
                            after = after.max(payload.latest_sequence);
                        }
                        Ok(IndexerStream::Event { payload }) => apply(&state, &payload, &mut after),
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

fn apply(state: &AppState, ev: &IndexedEvent, after: &mut u64) {
    state.ingest_event(ev);
    *after = (*after).max(ev.sequence);
}
