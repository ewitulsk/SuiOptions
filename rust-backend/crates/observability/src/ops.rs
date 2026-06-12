//! Minimal HTTP server for `/health` + `/metrics`.
//!
//! Drop-in upgrade of `runtime_config::health::spawn` for services that have
//! no axum stack (indexer ingest, mm-bot, option-scheduler, quoting-service,
//! balance-monitor): same tiny TCP loop, plus the Prometheus scrape endpoint.
//! axum services should mount [`crate::middleware::metrics_route`] instead.

use std::net::SocketAddr;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tracing::{debug, info};

const OK_RESP: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
const NOT_FOUND: &[u8] =
    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// Spawn a background task serving `GET /health` and `GET /metrics` on `addr`.
pub fn spawn(addr: SocketAddr) {
    tokio::spawn(async move {
        if let Err(e) = serve(addr).await {
            tracing::error!(error = %e, "ops server exited");
        }
    });
    info!(%addr, "ops server spawned (/health, /metrics)");
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

            match path {
                "/health" => {
                    let _ = socket.write_all(OK_RESP).await;
                }
                "/metrics" => {
                    let body = crate::metrics::render();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                }
                _ => {
                    let _ = socket.write_all(NOT_FOUND).await;
                }
            }
            let _ = socket.shutdown().await;
            debug!(path, "ops request");
        });
    }
}
