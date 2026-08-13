//! Mirror Binance's public flat-file dumps into bronze (spec §6.6):
//! list → diff → download → sha256-verify → upload verbatim. Idempotent
//! and resumable by construction — re-running only fetches what's missing.

use std::collections::HashSet;

use anyhow::Context;
use clap::Parser;
use futures::TryStreamExt;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

const VISION_LIST: &str = "https://s3-ap-northeast-1.amazonaws.com/data.binance.vision";
const VISION_GET: &str = "https://data.binance.vision";

#[derive(Parser)]
struct Cli {
    /// s3://bucket or file:///path lake root.
    #[arg(long, env = "STORE_URL")]
    store_url: String,
    #[arg(long, default_value = "spot")]
    market: String,
    /// Binance native symbols, comma-separated.
    #[arg(long, value_delimiter = ',', default_value = "BTCUSDC")]
    symbols: Vec<String>,
    /// Dump kinds to mirror (only `trades` is normalized; aggTrades is
    /// archive-only, spec §6.6).
    #[arg(long, value_delimiter = ',', default_value = "trades,aggTrades")]
    kinds: Vec<String>,
    /// Also mirror daily files (the seam between the last monthly dump
    /// and live capture).
    #[arg(long, default_value_t = true)]
    daily: bool,
    /// Only mirror files whose date part is >= this ISO prefix
    /// (e.g. "2024-01" or "2026-08-01"). Default: everything.
    #[arg(long)]
    since: Option<String>,
}

/// Date part of a dump filename: "BTCUSDC-trades-2026-08-10.zip" → "2026-08-10".
fn file_date(name: &str) -> Option<&str> {
    name.strip_suffix(".zip")?.splitn(3, '-').nth(2)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    let store = store::open(&cli.store_url)?;
    let http = reqwest::Client::builder()
        .user_agent("data-room-vision-sync")
        .build()?;

    let mut fetched = 0usize;
    for symbol in &cli.symbols {
        for kind in &cli.kinds {
            let mut periods = vec!["monthly"];
            if cli.daily {
                periods.push("daily");
            }
            for period in periods {
                let src_prefix = format!("data/{}/{}/{}/{}/", cli.market, period, kind, symbol);
                let dst_prefix = schema::bronze_vision_prefix(&cli.market, kind, symbol);

                let remote = list_vision(&http, &src_prefix).await?;
                let have = list_ours(&store, &dst_prefix).await?;

                for key in remote.iter().filter(|k| k.ends_with(".zip")) {
                    let name = key.rsplit('/').next().unwrap();
                    if have.contains(name) {
                        continue;
                    }
                    if let Some(since) = &cli.since {
                        // ISO date strings compare lexicographically; a
                        // monthly file "2026-08" passes since >= "2026-08-01"
                        // comparisons truncate to the shorter prefix.
                        let d = file_date(name).unwrap_or("");
                        let cmp_len = d.len().min(since.len());
                        if d[..cmp_len] < since[..cmp_len] {
                            continue;
                        }
                    }
                    match mirror_one(&http, &store, key, &format!("{dst_prefix}{name}")).await {
                        Ok(bytes) => {
                            fetched += 1;
                            info!(name, bytes, "mirrored");
                        }
                        Err(e) => warn!(name, "skipping: {e:#}"),
                    }
                }
            }
        }
    }
    info!(fetched, "vision-sync complete");
    Ok(())
}

/// List a vision-bucket prefix (S3 ListObjectsV2 XML, paginated).
async fn list_vision(http: &reqwest::Client, prefix: &str) -> anyhow::Result<Vec<String>> {
    let mut keys = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut url = format!("{VISION_LIST}?list-type=2&prefix={prefix}&max-keys=1000");
        if let Some(t) = &token {
            url.push_str(&format!("&continuation-token={}", urlencode(t)));
        }
        let body = http
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        keys.extend(extract_tags(&body, "Key"));
        token = extract_tags(&body, "NextContinuationToken")
            .into_iter()
            .next();
        if token.is_none() {
            break;
        }
    }
    Ok(keys)
}

/// Minimal XML tag extraction — the listing schema is flat and stable,
/// not worth an XML dependency.
fn extract_tags(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    xml.split(&open)
        .skip(1)
        .filter_map(|s| s.split(&close).next().map(str::to_string))
        .collect()
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

async fn list_ours(
    store: &std::sync::Arc<dyn object_store::ObjectStore>,
    prefix: &str,
) -> anyhow::Result<HashSet<String>> {
    let path = object_store::path::Path::from(prefix.trim_end_matches('/'));
    let names = store
        .list(Some(&path))
        .map_ok(|m| m.location.filename().unwrap_or_default().to_string())
        .try_collect::<HashSet<_>>()
        .await?;
    Ok(names)
}

/// Download one dump file, verify its published sha256, upload verbatim.
async fn mirror_one(
    http: &reqwest::Client,
    store: &std::sync::Arc<dyn object_store::ObjectStore>,
    vision_key: &str,
    dst_key: &str,
) -> anyhow::Result<usize> {
    let body = http
        .get(format!("{VISION_GET}/{vision_key}"))
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await
        .context("download")?;

    // CHECKSUM sidecar: "<sha256hex>  <filename>"
    let sidecar = http
        .get(format!("{VISION_GET}/{vision_key}.CHECKSUM"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let expected = sidecar
        .split_whitespace()
        .next()
        .context("empty CHECKSUM")?;
    let actual = hex::encode(Sha256::digest(&body));
    anyhow::ensure!(
        actual == expected,
        "checksum mismatch: {actual} != {expected}"
    );

    let n = body.len();
    store
        .put(&object_store::path::Path::from(dst_key), body.into())
        .await
        .context("upload")?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_keys_from_listing_xml() {
        let xml = "<r><Contents><Key>a/b.zip</Key><Size>1</Size></Contents>\
                   <Contents><Key>a/c.zip</Key></Contents><NextContinuationToken>tok</NextContinuationToken></r>";
        assert_eq!(extract_tags(xml, "Key"), vec!["a/b.zip", "a/c.zip"]);
        assert_eq!(extract_tags(xml, "NextContinuationToken"), vec!["tok"]);
    }

    #[test]
    fn file_date_extracts_iso_part() {
        assert_eq!(
            file_date("BTCUSDC-trades-2026-08-10.zip"),
            Some("2026-08-10")
        );
        assert_eq!(file_date("BTCUSDC-trades-2019-05.zip"), Some("2019-05"));
        assert_eq!(file_date("garbage"), None);
    }

    #[test]
    fn urlencode_escapes_non_unreserved() {
        assert_eq!(urlencode("a+b/c"), "a%2Bb%2Fc");
        assert_eq!(urlencode("token-1_2.3~"), "token-1_2.3~");
    }
}
