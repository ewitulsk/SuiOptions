//! WebSocket transport layer.
//!
//! Single bind address for retail and MM connections. The first frame is a
//! `Hello`; we peek at it to decide which handler to dispatch:
//!
//! - The MM `Hello` payload carries `account_id` and `signing_pubkey` (§5.4.5).
//! - The retail `Hello` payload carries `role` and `version` (§5.4.3).
//!
//! Anything else is treated as malformed and closed.

pub mod auth;
pub mod mm;
pub mod retail;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tracing::{debug, info};

use crate::AppState;
use crate::Config;

pub async fn serve(addr: SocketAddr, state: Arc<AppState>, cfg: Arc<Config>) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "quoting-service ws listening");
    serve_on(listener, state, cfg).await
}

/// Like [`serve`] but takes a pre-bound listener — used by tests that need
/// to discover the ephemeral port before connecting.
pub async fn serve_on(
    listener: TcpListener,
    state: Arc<AppState>,
    cfg: Arc<Config>,
) -> Result<()> {
    loop {
        let (socket, peer) = listener.accept().await?;
        let state = Arc::clone(&state);
        let cfg = Arc::clone(&cfg);
        tokio::spawn(async move {
            if let Err(e) = handle(socket, peer, state, cfg).await {
                debug!(?peer, error = %e, "client closed");
            }
        });
    }
}

async fn handle(
    socket: TcpStream,
    peer: SocketAddr,
    state: Arc<AppState>,
    cfg: Arc<Config>,
) -> Result<()> {
    let ws = tokio_tungstenite::accept_async(socket).await?;
    accept_handshake(ws, peer, state, cfg).await
}

/// Public entry point that takes an already-upgraded `WebSocketStream` —
/// used by tests where the listener and accept loop are owned outside this
/// crate.
pub async fn accept_handshake(
    ws: WebSocketStream<TcpStream>,
    peer: SocketAddr,
    state: Arc<AppState>,
    cfg: Arc<Config>,
) -> Result<()> {
    dispatch(ws, peer, state, cfg).await
}

async fn dispatch(
    mut ws: WebSocketStream<TcpStream>,
    peer: SocketAddr,
    state: Arc<AppState>,
    cfg: Arc<Config>,
) -> Result<()> {
    // First frame is a Hello.
    let first = ws
        .next()
        .await
        .ok_or_else(|| anyhow!("client closed before Hello"))??;
    let text = match first {
        Message::Text(t) => t,
        Message::Binary(b) => String::from_utf8(b)?,
        other => return Err(anyhow!("unexpected first frame: {:?}", other)),
    };

    let v: serde_json::Value = serde_json::from_str(&text)?;
    let typ = v
        .get("type")
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow!("missing type"))?;
    if typ != "Hello" {
        return Err(anyhow!("expected Hello, got {}", typ));
    }
    let payload = v.get("payload").ok_or_else(|| anyhow!("missing payload"))?;

    if payload.get("account_id").is_some() {
        // MM Hello shape.
        let hello: shared::protocol_types::messages::MmHelloPayload =
            serde_json::from_value(payload.clone())?;
        debug!(?peer, account = %hello.account_id, "mm hello");
        mm::handle(ws, peer, state, cfg, hello).await
    } else {
        let hello: shared::protocol_types::messages::RetailHelloPayload =
            serde_json::from_value(payload.clone())?;
        debug!(?peer, role = ?hello.role, "retail hello");
        retail::handle(ws, peer, state, cfg, hello).await
    }
}
