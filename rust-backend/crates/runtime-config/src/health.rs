//! Lightweight HTTP health-check server.
//!
//! Every backend service spawns one via [`spawn`]. It binds a TCP listener
//! on `addr` and responds to `GET /health` with `200 ok`. Any other path
//! returns 404. No dependencies beyond `tokio` — intentionally minimal so
//! services that don't otherwise need an HTTP stack still get a health
//! endpoint for ALB target-group checks and the uptime dashboard.

use std::net::SocketAddr;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tracing::{debug, info};

const OK_RESP: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
const NOT_FOUND: &[u8] =
    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// Spawn a background tokio task that serves a `/health` endpoint on `addr`.
pub fn spawn(addr: SocketAddr) {
    tokio::spawn(async move {
        if let Err(e) = serve(addr).await {
            tracing::error!(error = %e, "health server exited");
        }
    });
    info!(%addr, "health server spawned");
}

async fn serve(addr: SocketAddr) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    loop {
        let (mut socket, _peer) = listener.accept().await?;
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let n = match socket.peek(&mut buf).await {
                Ok(n) => n,
                Err(_) => return,
            };
            let text = std::str::from_utf8(&buf[..n]).unwrap_or("");
            let path = text.split_whitespace().nth(1).unwrap_or("/");

            // Drain the peeked data.
            let mut drain = [0u8; 1024];
            let _ = socket.try_read(&mut drain);

            let resp = if path == "/health" {
                OK_RESP
            } else {
                NOT_FOUND
            };
            let _ = socket.write_all(resp).await;
            let _ = socket.shutdown().await;
            debug!(path, "health request");
        });
    }
}
