//! Server-Sent-Events subscriber for `/v2/updates/price/stream`.
//!
//! Hermes streams one JSON envelope per SSE `data:` frame. We hand-roll
//! the SSE parsing on top of `reqwest::Response::bytes_stream()` because
//! the wire format is trivial (each event is `data: {json}\n\n`) and
//! avoiding another dep keeps the workspace lean.
//!
//! The subscriber owns a tokio task that:
//!   1. Opens the SSE connection.
//!   2. Streams `PriceUpdate`s back to a tokio mpsc channel.
//!   3. Reconnects with exponential backoff on any failure (Hermes
//!      force-closes the connection every 24h).
//!   4. Emits [`StreamEvent::Disconnected`] when it gives up trying to
//!      reconnect, so the consumer can gate trading on stale data.

use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use reqwest::Client;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::types::{HermesEnvelope, PriceFeedId, PriceUpdate};

const MIN_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const CHANNEL_DEPTH: usize = 1024;

/// If a connection was alive for at least this long before erroring, the
/// session was productive (data was flowing) and the error is a normal
/// server-side disconnect (e.g. HTTP/2 INTERNAL_ERROR from Hermes).
/// In that case we reset the backoff instead of growing it.
const HEALTHY_DURATION: Duration = Duration::from_secs(5);

/// Event delivered to the consumer. Most of the time it's a [`Price`]
/// update; a [`Disconnected`] notice fires after the reconnect loop has
/// been failing for `since.elapsed()` so the cache layer can mark feeds
/// stale.
#[derive(Debug)]
pub enum StreamEvent {
    Price(PriceUpdate),
    /// Reconnection is in progress; emitted at most once per outage.
    Disconnected { reason: String },
    /// Reconnected after an outage.
    Reconnected,
}

/// Spawn the SSE task. Returns the receiver end of an mpsc channel.
/// The task lives until either the channel is closed (receiver dropped)
/// or the task panics.
pub fn spawn_subscriber(
    client: Client,
    hermes_url: String,
    ids: Vec<PriceFeedId>,
) -> mpsc::Receiver<StreamEvent> {
    let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);
    tokio::spawn(run_loop(client, hermes_url, ids, tx));
    rx
}

async fn run_loop(
    client: Client,
    hermes_url: String,
    ids: Vec<PriceFeedId>,
    tx: mpsc::Sender<StreamEvent>,
) {
    let url = format!("{}/v2/updates/price/stream", hermes_url.trim_end_matches('/'));
    let query: Vec<(&str, String)> = ids.iter().map(|i| ("ids[]", i.to_hex())).collect();
    let mut backoff = MIN_BACKOFF;
    let mut was_disconnected = false;

    loop {
        let started = tokio::time::Instant::now();
        match connect_and_pump(&client, &url, &query, &tx).await {
            Ok(()) => {
                // Hermes closed cleanly (24h cap). Reconnect immediately.
                debug!(url, "pyth sse stream ended cleanly; reconnecting");
                backoff = MIN_BACKOFF;
            }
            Err(e) => {
                let alive = started.elapsed();
                let reason = format!("{e:#}");

                if alive >= HEALTHY_DURATION {
                    // Connection was productive — the error is a normal
                    // server-side disconnect (e.g. HTTP/2 INTERNAL_ERROR).
                    // Reset backoff so we reconnect quickly.
                    info!(url, alive_secs = alive.as_secs(), "pyth sse stream interrupted after healthy session; reconnecting");
                    backoff = MIN_BACKOFF;
                } else {
                    warn!(url, error = %reason, "pyth sse stream errored; reconnecting");
                    if !was_disconnected {
                        let _ = tx
                            .send(StreamEvent::Disconnected {
                                reason: reason.clone(),
                            })
                            .await;
                        was_disconnected = true;
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
            }
        }

        if was_disconnected {
            let _ = tx.send(StreamEvent::Reconnected).await;
            was_disconnected = false;
        }
    }
}

/// Hold one SSE connection open and pump parsed events into `tx`. Returns
/// when the upstream closes (Ok) or any error happens (Err).
async fn connect_and_pump(
    client: &Client,
    url: &str,
    query: &[(&str, String)],
    tx: &mpsc::Sender<StreamEvent>,
) -> Result<()> {
    info!(url, ids = query.len(), "opening pyth sse stream");
    let resp = client
        .get(url)
        .query(query)
        .header("accept", "text/event-stream")
        .send()
        .await
        .context("opening pyth sse connection")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "pyth sse handshake → HTTP {status}: {}",
            body.chars().take(500).collect::<String>()
        );
    }
    let mut body = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);

    while let Some(chunk) = body.next().await {
        let chunk = chunk.context("reading pyth sse chunk")?;
        buf.extend_from_slice(&chunk);

        // Emit each complete event. Events are separated by a blank line
        // (`\n\n` or `\r\n\r\n`).
        loop {
            let Some(end) = find_event_boundary(&buf) else {
                break;
            };
            let event = buf.drain(..end.0).collect::<Vec<u8>>();
            buf.drain(..end.1 - end.0); // skip the boundary itself
            if let Some(json) = extract_data_field(&event) {
                match serde_json::from_str::<HermesEnvelope>(&json) {
                    Ok(env) => {
                        for upd in env.parsed {
                            if tx.send(StreamEvent::Price(upd)).await.is_err() {
                                // Receiver gone — nothing to do but exit.
                                return Ok(());
                            }
                        }
                    }
                    Err(e) => {
                        debug!(
                            error = %e,
                            preview = %json.chars().take(200).collect::<String>(),
                            "pyth sse: failed to parse data line as HermesEnvelope"
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

/// Returns `(payload_end_idx, boundary_end_idx)` for the first complete
/// SSE event in `buf`, or `None` if no boundary is present yet. The
/// payload occupies `buf[..payload_end]`; the next read should start at
/// `buf[boundary_end..]`.
fn find_event_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    // Look for `\n\n` first (the spec form), then `\r\n\r\n`.
    if let Some(p) = buf.windows(2).position(|w| w == b"\n\n") {
        return Some((p, p + 2));
    }
    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
        return Some((p, p + 4));
    }
    None
}

/// Concatenate all `data:` lines in an SSE event into a single JSON
/// string. Per the SSE spec, multi-line `data:` payloads are joined with
/// `\n`; Pyth emits one-line payloads in practice.
fn extract_data_field(event: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(event).ok()?;
    let mut out = String::new();
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("data:") {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(rest.trim_start());
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary_finds_double_newline() {
        let s = b"data: {\"a\":1}\n\n";
        let (p, b) = find_event_boundary(s).unwrap();
        assert_eq!(&s[..p], b"data: {\"a\":1}");
        assert_eq!(b, s.len());
    }

    #[test]
    fn boundary_handles_crlf() {
        let s = b"data: x\r\n\r\nrest";
        let (p, b) = find_event_boundary(s).unwrap();
        assert_eq!(&s[..p], b"data: x");
        assert_eq!(&s[b..], b"rest");
    }

    #[test]
    fn extract_concatenates_data_lines() {
        let evt = b"event: price\ndata: first\ndata: second\n";
        assert_eq!(extract_data_field(evt).unwrap(), "first\nsecond");
    }

    #[test]
    fn extract_ignores_non_data_lines() {
        let evt = b"event: price\nid: 42\ndata: {\"x\":1}\n";
        assert_eq!(extract_data_field(evt).unwrap(), "{\"x\":1}");
    }
}
