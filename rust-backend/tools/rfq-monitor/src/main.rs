//! Subscribes to the quoting service's `/rfq-stream` NDJSON feed and prints
//! one line per incoming retail RFQRequest.
//!
//! The endpoint streams `RfqObservation` records (defined in the
//! quoting service's `state` module). Each line is JSON:
//!
//! ```json
//! {"timestamp_ms":..., "request_id":"...", "bucket_id":"0x...",
//!  "write_amount":..., "side":"writer"}
//! ```
//!
//! A synthetic `{"_lagged":N}` line means the server dropped N messages
//! because we couldn't keep up — should never happen in practice unless
//! something has wedged this process.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use url::Url;

use rfq_monitor::Cli;

#[derive(Debug, Deserialize)]
struct RfqObservation {
    timestamp_ms: u64,
    request_id: String,
    bucket_id: String,
    write_amount: u64,
    side: String,
}

#[derive(Debug, Deserialize)]
struct Lagged {
    #[serde(rename = "_lagged")]
    lagged: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    runtime_config::logging::init();
    let cli = Cli::parse();

    let url = Url::parse(&cli.url).with_context(|| format!("parse url {}", cli.url))?;
    if url.scheme() != "http" {
        return Err(anyhow!(
            "only plain http:// is supported (got scheme {:?})",
            url.scheme()
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("url has no host: {}", cli.url))?
        .to_string();
    let port = url.port().unwrap_or(80);
    let path = if url.path().is_empty() { "/" } else { url.path() };

    eprintln!("rfq-monitor → {}:{}{}", host, port, path);

    let mut sock = TcpStream::connect((host.as_str(), port))
        .await
        .with_context(|| format!("connect to {host}:{port}"))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nAccept: application/x-ndjson\r\n\r\n"
    );
    sock.write_all(request.as_bytes()).await?;

    let mut reader = BufReader::new(sock);

    // Consume HTTP status + headers up to the blank line.
    let mut status_line = String::new();
    reader.read_line(&mut status_line).await?;
    if !status_line.starts_with("HTTP/1.1 200") {
        return Err(anyhow!("server returned {}", status_line.trim()));
    }
    loop {
        let mut header = String::new();
        let n = reader.read_line(&mut header).await?;
        if n == 0 || header == "\r\n" || header == "\n" {
            break;
        }
    }

    // Stream NDJSON.
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            eprintln!("server closed the stream");
            break;
        }
        let trimmed = line.trim_end_matches(|c| c == '\r' || c == '\n');
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(lag) = serde_json::from_str::<Lagged>(trimmed) {
            eprintln!("! dropped {} observations (rfq-monitor falling behind)", lag.lagged);
            continue;
        }
        match serde_json::from_str::<RfqObservation>(trimmed) {
            Ok(obs) => print(&obs),
            Err(e) => eprintln!("bad line: {e} — {trimmed}"),
        }
    }
    Ok(())
}

fn print(o: &RfqObservation) {
    let ts = format_ts(o.timestamp_ms);
    println!(
        "{ts}  {side:6}  bucket={bucket}  write={amount}  req={req}",
        side = o.side,
        bucket = short(&o.bucket_id),
        amount = o.write_amount,
        req = o.request_id,
    );
}

fn format_ts(ms: u64) -> String {
    use chrono::{DateTime, Utc};
    let secs = (ms / 1000) as i64;
    let nanos = ((ms % 1000) * 1_000_000) as u32;
    DateTime::<Utc>::from_timestamp(secs, nanos)
        .unwrap_or_else(|| {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
            DateTime::<Utc>::from_timestamp(now.as_secs() as i64, 0).unwrap()
        })
        .format("%H:%M:%S%.3f")
        .to_string()
}

/// `0x0123…cdef` — first 6 + last 4 hex chars, easier on the eye than a
/// 66-char ObjectId in a terminal tail.
fn short(id: &str) -> String {
    let s = id.trim_start_matches("0x");
    if s.len() <= 12 {
        return format!("0x{s}");
    }
    let head = &s[..6];
    let tail = &s[s.len() - 4..];
    format!("0x{head}…{tail}")
}
