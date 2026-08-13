//! Live capture daemon (spec §6): websocket → verbatim spool → hourly
//! gzip upload to bronze. The only long-lived process in the data room.

mod spool;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use rand::RngCore;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use spool::Spool;

#[derive(Parser)]
struct Cli {
    /// Path to collector.toml
    #[arg(long, env = "COLLECTOR_CONFIG", default_value = "collector.toml")]
    config: PathBuf,
}

#[derive(Deserialize, Clone)]
struct Config {
    /// s3://bucket or file:///path lake root.
    store_url: String,
    spool_dir: PathBuf,
    #[serde(default = "default_metrics_listen")]
    metrics_listen: String,
    #[serde(default = "default_max_file_mb")]
    max_file_mb: u64,
    /// Reconnect if no websocket traffic for this long.
    #[serde(default = "default_stall_secs")]
    stall_secs: u64,
    connections: Vec<Connection>,
}

#[derive(Deserialize, Clone)]
struct Connection {
    /// Only "coinbase" is implemented; the field keeps config shape
    /// venue-generic (spec §7.1).
    exchange: String,
    url: String,
    products: Vec<String>,
    channels: Vec<String>,
}

fn default_metrics_listen() -> String {
    "0.0.0.0:9100".into()
}
fn default_max_file_mb() -> u64 {
    512
}
fn default_stall_secs() -> u64 {
    30
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos() as i64
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    let cfg: Config = toml::from_str(&std::fs::read_to_string(&cli.config)?)
        .with_context(|| format!("parsing {:?}", cli.config))?;

    metrics_exporter_prometheus::PrometheusBuilder::new()
        .with_http_listener(cfg.metrics_listen.parse::<std::net::SocketAddr>()?)
        .install()?;

    let mut boot = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut boot);
    let boot_id = hex::encode(boot);
    info!(boot_id, "collector starting");

    let store = store::open(&cfg.store_url)?;
    let spool = Arc::new(Mutex::new(Spool::new(
        &cfg.spool_dir,
        boot_id,
        cfg.max_file_mb * 1024 * 1024,
    )));

    let (upload_tx, mut upload_rx) = mpsc::unbounded_channel::<PathBuf>();

    // Boot sweep: anything a previous boot left behind.
    for f in spool.lock().unwrap().stale_files() {
        let _ = upload_tx.send(f);
    }

    // Uploader. Failed uploads stay on disk; the periodic stale sweep
    // retries them, and dataroom-upload-stalled fires if they age out.
    {
        let store = store.clone();
        let spool_root = cfg.spool_dir.clone();
        tokio::spawn(async move {
            while let Some(f) = upload_rx.recv().await {
                if let Err(e) = spool::upload(&store, &spool_root, &f).await {
                    error!(alert_id = "dataroom-upload-stalled", file = ?f, "upload failed: {e:#}");
                }
            }
        });
    }

    // Rotation + stale sweep ticker.
    {
        let spool = spool.clone();
        let upload_tx = upload_tx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            let mut sweeps: u32 = 0;
            loop {
                tick.tick().await;
                let closed = {
                    let mut s = spool.lock().unwrap();
                    let mut c = s.rotate_expired(now_ns()).unwrap_or_default();
                    sweeps += 1;
                    if sweeps.is_multiple_of(10) {
                        c.extend(s.stale_files()); // retry failed uploads
                    }
                    c
                };
                for f in closed {
                    let _ = upload_tx.send(f);
                }
            }
        });
    }

    // One capture task per connection.
    let mut capture_tasks = Vec::new();
    for conn in cfg.connections.clone() {
        if conn.exchange != "coinbase" {
            anyhow::bail!("unsupported exchange {}", conn.exchange);
        }
        let spool = spool.clone();
        let upload_tx = upload_tx.clone();
        let stall = Duration::from_secs(cfg.stall_secs);
        capture_tasks.push(tokio::spawn(async move {
            run_connection(conn, spool, upload_tx, stall).await;
        }));
    }

    // Graceful shutdown: stop capture first so nothing reopens spool
    // files mid-flush, then rotate everything and upload synchronously.
    shutdown_signal().await;
    info!("shutting down: flushing spool");
    for t in &capture_tasks {
        t.abort();
    }
    let files = {
        let mut s = spool.lock().unwrap();
        s.close_all().ok();
        s.stale_files() // now includes everything just closed, deduped
    };
    for f in files {
        if let Err(e) = spool::upload(&store, &cfg.spool_dir, &f).await {
            error!(alert_id = "dataroom-upload-stalled", file = ?f, "final upload failed: {e:#}");
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await.ok();
}

/// Marker streams for a connection: every (product × data channel) pair,
/// so the gaps job sees boundaries on exactly the streams it audits.
fn marker_streams(conn: &Connection) -> Vec<String> {
    let mut out = Vec::new();
    for p in &conn.products {
        for c in &conn.channels {
            match c.as_str() {
                "matches" => out.push(format!("matches.{p}")),
                "ticker" => out.push(format!("ticker.{p}")),
                _ => {}
            }
        }
    }
    out
}

async fn run_connection(
    conn: Connection,
    spool: Arc<Mutex<Spool>>,
    upload_tx: mpsc::UnboundedSender<PathBuf>,
    stall: Duration,
) {
    let seq = AtomicU64::new(0);
    let mut backoff = Duration::from_secs(1);
    loop {
        match capture_once(&conn, &spool, &upload_tx, &seq, stall).await {
            Ok(()) => backoff = Duration::from_secs(1), // clean EOF: reconnect fast
            Err(e) => {
                warn!(exchange = conn.exchange, "connection error: {e:#}");
                metrics::counter!("dataroom_collector_reconnects_total", "exchange" => conn.exchange.clone()).increment(1);
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
        // Disconnect marker on every exit path.
        write_markers(&conn, &spool, &upload_tx, &seq, "disconnect");
        tokio::time::sleep(backoff.mul_f64(0.5 + rand::random::<f64>() * 0.5)).await;
    }
}

fn write_markers(
    conn: &Connection,
    spool: &Arc<Mutex<Spool>>,
    upload_tx: &mpsc::UnboundedSender<PathBuf>,
    seq: &AtomicU64,
    event: &str,
) {
    let ts = now_ns();
    let mut s = spool.lock().unwrap();
    for stream in marker_streams(conn) {
        let n = seq.fetch_add(1, Ordering::Relaxed);
        match s.write(&conn.exchange, &stream, ts, n, None, Some(event)) {
            Ok(Some(closed)) => {
                let _ = upload_tx.send(closed);
            }
            Ok(None) => {}
            Err(e) => error!("marker write failed: {e:#}"),
        }
    }
}

async fn capture_once(
    conn: &Connection,
    spool: &Arc<Mutex<Spool>>,
    upload_tx: &mpsc::UnboundedSender<PathBuf>,
    seq: &AtomicU64,
    stall: Duration,
) -> anyhow::Result<()> {
    let (mut ws, _) = tokio_tungstenite::connect_async(&conn.url).await?;
    let sub = serde_json::json!({
        "type": "subscribe",
        "product_ids": conn.products,
        "channels": conn.channels,
    });
    ws.send(Message::Text(sub.to_string())).await?;
    info!(
        exchange = conn.exchange,
        url = conn.url,
        "connected + subscribed"
    );
    write_markers(conn, spool, upload_tx, seq, "connect");

    loop {
        let msg = match tokio::time::timeout(stall, ws.next()).await {
            Err(_) => anyhow::bail!("stalled: no traffic for {stall:?}"),
            Ok(None) => return Ok(()), // clean close
            Ok(Some(m)) => m?,
        };
        match msg {
            Message::Text(payload) => {
                let ts = now_ns();
                let stream = adapters::coinbase::route(&payload)
                    .unwrap_or_else(|| format!("control.{}", conn.exchange));
                let n = seq.fetch_add(1, Ordering::Relaxed);
                metrics::counter!("dataroom_collector_messages_total",
                    "exchange" => conn.exchange.clone(), "stream" => stream.clone())
                .increment(1);
                metrics::gauge!("dataroom_collector_last_message_unix_seconds",
                    "exchange" => conn.exchange.clone())
                .set(ts as f64 / 1e9);
                let closed = {
                    let mut s = spool.lock().unwrap();
                    s.write(&conn.exchange, &stream, ts, n, Some(&payload), None)?
                };
                if let Some(f) = closed {
                    let _ = upload_tx.send(f);
                }
            }
            Message::Ping(p) => ws.send(Message::Pong(p)).await?,
            Message::Close(_) => return Ok(()),
            _ => {}
        }
    }
}
