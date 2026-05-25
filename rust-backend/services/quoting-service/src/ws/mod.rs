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
use tokio::io::AsyncWriteExt;
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
    // Peek the request preamble without consuming it. ALB health checks send
    // a plain `GET /health HTTP/1.1` with no `Upgrade: websocket` header;
    // tungstenite would reject those as malformed handshakes. WS upgrades
    // pass through untouched.
    if let Some(req) = peek_http_request(&socket).await? {
        if !req.is_ws_upgrade {
            return handle_plain_http(socket, &req, state).await;
        }
    }
    let ws = tokio_tungstenite::accept_async(socket).await?;
    accept_handshake(ws, peer, state, cfg).await
}

struct PeekedRequest {
    path: String,
    is_ws_upgrade: bool,
}

/// Best-effort peek at the first ~1KB of the TCP stream to identify the
/// HTTP request line and headers. Returns `None` if not enough data has
/// arrived yet (the caller falls through to `accept_async`, which will
/// block on its own read). Returns `Some` once we can see the request
/// line and at least up to a blank-line terminator or 1KB worth of header.
async fn peek_http_request(socket: &TcpStream) -> Result<Option<PeekedRequest>> {
    let mut buf = [0u8; 1024];
    let n = socket.peek(&mut buf).await?;
    if n == 0 {
        return Ok(None);
    }
    let text = match std::str::from_utf8(&buf[..n]) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let header_end = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(text.len());
    let head = &text[..header_end];
    let mut lines = head.split("\r\n");
    let request_line = match lines.next() {
        Some(l) if !l.is_empty() => l,
        _ => return Ok(None),
    };
    let mut parts = request_line.split_whitespace();
    let _method = parts.next();
    let path = parts.next().unwrap_or("/").to_string();
    let is_ws_upgrade = lines.any(|l| {
        let lower = l.to_ascii_lowercase();
        lower.starts_with("upgrade:") && lower.contains("websocket")
    });
    Ok(Some(PeekedRequest { path, is_ws_upgrade }))
}

async fn handle_plain_http(
    mut socket: TcpStream,
    req: &PeekedRequest,
    state: Arc<AppState>,
) -> Result<()> {
    // Drain the request bytes we peeked. ALB sends them in one packet and
    // doesn't expect us to read the body; reading 1KB is enough to clear
    // the kernel buffer for the close.
    let mut drain = [0u8; 1024];
    let _ = socket.try_read(&mut drain);

    if req.path == "/rfq-stream" || req.path.ends_with("/rfq-stream") {
        return serve_rfq_stream(socket, state).await;
    }
    let body = if req.path == "/health" || req.path.ends_with("/health") {
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
            .to_vec()
    } else {
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
    };
    socket.write_all(&body).await?;
    socket.shutdown().await.ok();
    Ok(())
}

/// Stream `RfqObservation`s as newline-delimited JSON over a long-lived
/// HTTP response. Drives the `rfq-monitor` tool. Slow subscribers that fall
/// 256 messages behind get `Lagged` and we emit a synthetic `_lagged` line
/// so the operator can see drops without us silently swallowing them.
async fn serve_rfq_stream(mut socket: TcpStream, state: Arc<AppState>) -> Result<()> {
    socket
        .write_all(
            b"HTTP/1.1 200 OK\r\n\
              Content-Type: application/x-ndjson\r\n\
              Cache-Control: no-store\r\n\
              Connection: close\r\n\
              \r\n",
        )
        .await?;

    let mut rx = state.rfq_observers.subscribe();
    loop {
        match rx.recv().await {
            Ok(obs) => {
                let mut line = match serde_json::to_string(&obs) {
                    Ok(s) => s,
                    Err(e) => {
                        debug!(error = %e, "encode rfq observation");
                        continue;
                    }
                };
                line.push('\n');
                if socket.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                let line = format!("{{\"_lagged\":{n}}}\n");
                if socket.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
    socket.shutdown().await.ok();
    Ok(())
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
        let hello: protocol_types::messages::MmHelloPayload =
            serde_json::from_value(payload.clone())?;
        debug!(?peer, account = %hello.account_id, "mm hello");
        mm::handle(ws, peer, state, cfg, hello).await
    } else {
        let hello: protocol_types::messages::RetailHelloPayload =
            serde_json::from_value(payload.clone())?;
        debug!(?peer, role = ?hello.role, "retail hello");
        retail::handle(ws, peer, state, cfg, hello).await
    }
}
