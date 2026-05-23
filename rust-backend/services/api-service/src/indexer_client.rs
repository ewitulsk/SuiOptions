//! Subscribes to the indexer's WS fanout and feeds events into [`AppState`].
//!
//! Mirrors `quoting-service::indexer_client` — on connect, sends one
//! `Subscribe { after_sequence }`, drains the `Snapshot`, then pumps live
//! `Event` frames. Reconnects forever on disconnect; the indexer's snapshot
//! catch-up covers missed sequences within its ring-buffer window.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use shared::protocol_types::events::IndexedEvent;
use shared::protocol_types::messages::{IndexerStream, IndexerSubscribe};

use crate::state::AppState;

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
                            // TODO(SO-?): indexer fanout snapshot is bounded by
                            // `recent_log_capacity` (default 1024). If the gap
                            // between payload.latest_sequence and events.len()
                            // is large, we silently miss state. Track via Jira.
                            debug!(
                                events = payload.events.len(),
                                latest_sequence = payload.latest_sequence,
                                "indexer snapshot"
                            );
                            for e in &payload.events {
                                apply(&state, e, &mut after);
                            }
                            after = after.max(payload.latest_sequence);
                        }
                        Ok(IndexerStream::Event { payload }) => {
                            apply(&state, &payload, &mut after);
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

fn apply(state: &AppState, ev: &IndexedEvent, after: &mut u64) {
    state.ingest_event(ev);
    *after = (*after).max(ev.sequence);
}
