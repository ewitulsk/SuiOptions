//! Thin WebSocket wrapper for talking to solana-quoting-service — the
//! port of sui-tx's `ws_client` (connect + JSON send/recv helpers; the
//! binary layers its per-role state on top).

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{de::DeserializeOwned, Serialize};
use tokio::net::TcpStream;
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
    ws.send(Message::Text(text.into()))
        .await
        .context("sending WS frame")?;
    Ok(())
}

/// Read the next text/binary frame and JSON-decode it as `T`. Skips
/// control frames (Ping/Pong).
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
                let s = String::from_utf8(b.to_vec()).context("binary frame not utf-8")?;
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
