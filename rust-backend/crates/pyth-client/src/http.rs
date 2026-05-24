//! HTTP one-shots against Hermes and Benchmarks.
//!
//! Hermes endpoints used:
//!   - `GET /v2/updates/price/latest?ids[]=…` — latest cached price for
//!     one or many feeds, in a single call.
//!
//! Benchmarks endpoint:
//!   - `GET /v1/updates/price/{unix_seconds}?ids[]=…` — the closest price
//!     observation at or before the given timestamp.
//!
//! Both share the global 10-req-per-10-second rate cap (per source IP).
//! [`get_with_backoff`] retries on 429 with a one-minute pause and on
//! transient network errors with exponential backoff.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::{Client, StatusCode};
use tracing::{debug, warn};

use super::types::{HermesEnvelope, PriceFeedId, PriceUpdate};

const MAX_RETRIES: u32 = 5;
const INITIAL_BACKOFF_MS: u64 = 200;
const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Latest prices for one or more feeds. Returns the `parsed` array.
pub async fn latest(
    client: &Client,
    hermes_url: &str,
    ids: &[PriceFeedId],
) -> Result<Vec<PriceUpdate>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let url = format!("{}/v2/updates/price/latest", hermes_url.trim_end_matches('/'));
    let query: Vec<(&str, String)> = ids.iter().map(|i| ("ids[]", i.to_hex())).collect();
    let env: HermesEnvelope = get_with_backoff(client, &url, &query).await?;
    Ok(env.parsed)
}

/// One historical observation at-or-before `unix_seconds` for a single
/// feed. The Benchmarks API supports multi-id queries the same way Hermes
/// does, but the mm-bot only needs one feed per call for vol sampling.
pub async fn benchmark_at(
    client: &Client,
    benchmarks_url: &str,
    id: PriceFeedId,
    unix_seconds: i64,
) -> Result<PriceUpdate> {
    let url = format!(
        "{}/v1/updates/price/{}",
        benchmarks_url.trim_end_matches('/'),
        unix_seconds
    );
    let query = [("ids[]", id.to_hex())];
    let env: HermesEnvelope = get_with_backoff(client, &url, &query).await?;
    env.parsed
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("benchmarks returned no parsed entries for {id} @ {unix_seconds}"))
}

async fn get_with_backoff<Q, T>(client: &Client, url: &str, query: &Q) -> Result<T>
where
    Q: serde::Serialize + ?Sized,
    T: serde::de::DeserializeOwned,
{
    let mut backoff = INITIAL_BACKOFF_MS;
    for attempt in 1..=MAX_RETRIES {
        match client.get(url).query(query).timeout(REQUEST_TIMEOUT).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status == StatusCode::TOO_MANY_REQUESTS {
                    warn!(
                        attempt,
                        url,
                        "pyth rate-limited (429), sleeping {:?}", RATE_LIMIT_BACKOFF
                    );
                    tokio::time::sleep(RATE_LIMIT_BACKOFF).await;
                    continue;
                }
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(anyhow!(
                        "pyth GET {url} → HTTP {status}: {}",
                        body.chars().take(500).collect::<String>()
                    ));
                }
                let parsed = resp.json::<T>().await.context("decoding pyth json body")?;
                return Ok(parsed);
            }
            Err(e) if attempt < MAX_RETRIES => {
                debug!(attempt, error = %e, "pyth http transient error, backing off");
                tokio::time::sleep(Duration::from_millis(backoff)).await;
                backoff = (backoff * 2).min(5_000);
            }
            Err(e) => return Err(e).with_context(|| format!("pyth GET {url}")),
        }
    }
    Err(anyhow!("pyth GET {url} gave up after {MAX_RETRIES} attempts"))
}
