//! Minimal HTTP server for `/health` + `/metrics`.
//!
//! The health/metrics server for services that have no axum stack (indexer
//! ingest, mm-bot, option-scheduler, keeper, market-sim, balance-monitor): a
//! tiny TCP loop plus the Prometheus scrape endpoint. axum services should
//! mount [`crate::middleware::metrics_route`] instead.
//!
//! This replaced `runtime_config::health::spawn`, which was a near-identical
//! twin with the same unconditional 200 and a doc comment claiming every
//! backend service used it. It had zero callers and was deleted in SO-324
//! rather than taught about readiness — two health servers to keep in sync is
//! the shape that produced the second vacuous one in the first place.
//!
//! # `/health` reports readiness, not liveness (SO-324)
//!
//! [`spawn`] takes a [`Readiness`] handle and `/health` answers 503 until the
//! caller flips it with [`Readiness::ready`]. This exists because the previous
//! behaviour — 200 from the instant the listener bound — made the deploy
//! health gate vacuous: a service that served `/health` half a second into
//! startup and then died on a fallible call still passed, because
//! `deploy.sh` polls every 2s and breaks on the first success. mm-bot
//! deployed green while crash-looping on 2026-07-30 for exactly this reason.
//!
//! `/metrics` is unaffected and available immediately, so a service that
//! never becomes ready is still observable while it fails.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tracing::{debug, info};

const OK_RESP: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
/// `/health` before [`Readiness::ready`] is called. 503 rather than 200 is the
/// whole point of SO-324: `deploy.sh` uses `curl -fsS`, which fails on 5xx, so
/// the deploy gate keeps polling instead of accepting a process that has
/// started but not finished its fallible startup.
const STARTING_RESP: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: 8\r\nConnection: close\r\n\r\nstarting";
const NOT_FOUND: &[u8] =
    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// Handle for flipping `/health` from 503 to 200 once startup has finished.
///
/// Constructed by the caller in the **not-ready** state and handed to [`spawn`].
/// A service that never calls [`Readiness::ready`] never passes its deploy
/// health gate — that default is deliberate: forgetting fails loudly at deploy
/// time rather than silently certifying a half-started process (SO-324).
#[derive(Clone, Debug)]
pub struct Readiness(Arc<AtomicBool>);

impl Default for Readiness {
    fn default() -> Self {
        Self::new()
    }
}

impl Readiness {
    /// A fresh handle in the not-ready state.
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Mark the service ready. `/health` starts answering 200.
    ///
    /// Call this **after all fallible startup work**, immediately before the
    /// service's steady-state loop. Anything fallible left after this point is
    /// outside the gate's protection — `deploy.sh` breaks its poll on the
    /// first success, so a flip placed mid-startup re-creates the vacuous gate
    /// for whatever follows it.
    pub fn ready(&self) {
        if !self.0.swap(true, Ordering::SeqCst) {
            info!("startup complete; /health now reports ready");
        }
    }

    fn is_ready(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Spawn a background task serving `GET /health` and `GET /metrics` on `addr`.
///
/// `/health` answers **503 until [`Readiness::ready`] is called** on
/// `readiness`; `/metrics` is available immediately so startup itself stays
/// observable.
///
/// `readiness` is a required argument rather than something this function
/// creates and returns, and that is the point: an ignored return value still
/// compiles, so a caller could have kept the old vacuous behaviour by saying
/// nothing. Taking the handle makes every present and future caller state the
/// readiness decision, and makes omitting it a compile error (SO-324).
pub fn spawn(addr: SocketAddr, readiness: &Readiness) {
    let serve_readiness = readiness.clone();
    tokio::spawn(async move {
        if let Err(e) = serve(addr, serve_readiness).await {
            tracing::error!(error = %e, "ops server exited");
        }
    });
    info!(%addr, "ops server spawned (/health, /metrics); /health 503 until ready");
}

async fn serve(addr: SocketAddr, readiness: Readiness) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    loop {
        let (mut socket, _peer) = listener.accept().await?;
        let readiness = readiness.clone();
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
                    let resp = if readiness.is_ready() {
                        OK_RESP
                    } else {
                        STARTING_RESP
                    };
                    let _ = socket.write_all(resp).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpStream;

    /// A free loopback address. Bind-then-drop, and `get` below retries, so the
    /// gap between releasing the port and `serve` claiming it is tolerated.
    async fn free_addr() -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        addr
    }

    /// Issue `GET {path}` and return the response's status line.
    async fn get(addr: SocketAddr, path: &str) -> String {
        let mut last = String::new();
        for _ in 0..50 {
            let Ok(mut s) = TcpStream::connect(addr).await else {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                continue;
            };
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).await.unwrap();
            last = String::from_utf8_lossy(&buf).into_owned();
            if !last.is_empty() {
                return last.lines().next().unwrap_or("").to_string();
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("no response from {path}: {last:?}");
    }

    #[test]
    fn readiness_starts_not_ready_and_ready_is_idempotent() {
        let r = Readiness::new();
        assert!(!r.is_ready(), "a fresh Readiness must not be ready");
        r.ready();
        assert!(r.is_ready());
        r.ready();
        assert!(r.is_ready(), "ready() must be safe to call twice");
    }

    /// A clone must observe the original's flip — `serve` hands one clone per
    /// connection, so a per-connection copy that didn't share state would be
    /// permanently false.
    #[test]
    fn clones_share_state() {
        let r = Readiness::new();
        let c = r.clone();
        assert!(!c.is_ready());
        r.ready();
        assert!(c.is_ready(), "clone must see the flip through the Arc");
    }

    /// The constraint the deploy gate actually rests on: `deploy.sh` uses
    /// `curl -fsS`, which only fails on HTTP >= 400. A not-ready `/health` that
    /// answered 200 with a different body would pass the gate and leave the
    /// fix vacuous, so this asserts the status line, not the body.
    #[tokio::test]
    async fn health_is_503_until_ready_then_200() {
        let addr = free_addr().await;
        let readiness = Readiness::new();
        spawn(addr, &readiness);

        assert_eq!(
            get(addr, "/health").await,
            "HTTP/1.1 503 Service Unavailable",
            "/health must be >= 400 before ready"
        );

        readiness.ready();

        assert_eq!(
            get(addr, "/health").await,
            "HTTP/1.1 200 OK",
            "/health must be 200 once ready"
        );
    }

    /// `/metrics` shares the listener but must never be gated: a service stuck
    /// in startup is exactly when its metrics are most worth scraping.
    #[tokio::test]
    async fn metrics_is_available_while_not_ready() {
        let addr = free_addr().await;
        let readiness = Readiness::new();
        spawn(addr, &readiness);

        assert_eq!(get(addr, "/metrics").await, "HTTP/1.1 200 OK");
        assert!(!readiness.is_ready(), "still not ready — metrics is ungated");
    }

    /// Not-ready must be a 503 from a listening socket, not a refused
    /// connection: readiness is never implemented by delaying the bind, which
    /// would take `/metrics` down with it.
    #[tokio::test]
    async fn binds_before_ready() {
        let addr = free_addr().await;
        let readiness = Readiness::new();
        spawn(addr, &readiness);

        // /metrics answering proves the listener is up while /health is 503.
        assert_eq!(get(addr, "/metrics").await, "HTTP/1.1 200 OK");
        assert_eq!(get(addr, "/health").await, "HTTP/1.1 503 Service Unavailable");
    }
}
