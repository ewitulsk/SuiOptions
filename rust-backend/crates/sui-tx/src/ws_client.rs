//! Thin WebSocket wrapper for talking to the quoting service.
//!
//! Both retail and MM clients use the same envelope shapes
//! (`RetailToService` / `MmToService` etc. from `protocol-types`). This
//! module just exposes `connect` + send/recv helpers; the binaries layer
//! their per-role state on top.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{de::DeserializeOwned, Serialize};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, trace, warn};

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub async fn connect(url: &str) -> Result<WsStream> {
    debug!(url, "opening ws connection");
    let (ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .with_context(|| format!("connecting WS to {url}"))?;
    debug!(url, "ws connection established");
    Ok(ws)
}

pub async fn send_json<T: Serialize>(ws: &mut WsStream, msg: &T) -> Result<()> {
    let text = serde_json::to_string(msg).context("serialising WS frame")?;
    trace!(len = text.len(), "sending ws frame");
    ws.send(Message::Text(text)).await.context("sending WS frame")?;
    Ok(())
}

/// Read the next text/binary frame and JSON-decode it as `T`. Skips
/// control frames (Ping/Pong/Close-passthrough).
pub async fn next_json<T: DeserializeOwned>(ws: &mut WsStream) -> Result<T> {
    loop {
        let frame = ws
            .next()
            .await
            .ok_or_else(|| anyhow!("ws closed"))?
            .context("ws read error")?;
        match frame {
            Message::Text(t) => {
                trace!(len = t.len(), "received ws text frame");
                return serde_json::from_str(&t).context("decoding WS frame");
            }
            Message::Binary(b) => {
                trace!(len = b.len(), "received ws binary frame");
                let s = String::from_utf8(b).context("binary frame not utf-8")?;
                return serde_json::from_str(&s).context("decoding WS frame");
            }
            Message::Close(_) => {
                warn!("ws closed by peer");
                return Err(anyhow!("ws closed by peer"));
            }
            _ => continue,
        }
    }
}

/// Same as [`next_json`] but errors after `dur` rather than blocking
/// forever. Useful for RFQ responses where we wait at most ~2s.
pub async fn next_json_timeout<T: DeserializeOwned>(
    ws: &mut WsStream,
    dur: Duration,
) -> Result<T> {
    timeout(dur, next_json(ws))
        .await
        .map_err(|_| anyhow!("ws read timed out after {dur:?}"))?
}
