//! Cached, paced, bulk realized-vol from Pyth Benchmarks.
//!
//! [`realized_sigma_from_benchmarks`](crate::sigma::realized_sigma_from_benchmarks)
//! fetches one daily close per HTTP request, single feed, with no caching and
//! no inter-request pacing. A caller that recomputes vol on a tick (the
//! price-charting apy sampler, every `apy_tick_secs`) therefore re-fetches the
//! *same* historical closes every tick, once per vault — a steady 429 storm
//! against a public endpoint.
//!
//! [`BenchmarkVol`] fixes that with the three things the endpoint deserves:
//!
//!   - **Caching.** Daily closes are immutable once the day is in the past, so
//!     we key the cache by `(feed, day_boundary)` and never re-fetch a hit.
//!     Sampling timestamps are quantized to UTC midnight so the keys are
//!     stable across ticks — the bare `now - i*DAY` the free function uses
//!     drifts every call and would never hit.
//!   - **Bulk fetch.** The Benchmarks endpoint takes many `ids` per request,
//!     so a window is filled one request *per day* covering every feed that
//!     still needs that day, not one request per feed per day.
//!   - **Paced requests.** A single shared pacer serializes requests and
//!     spaces them by [`MIN_REQUEST_INTERVAL`], keeping the client under the
//!     ~10-req/10s public cap even on a cold cache.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use tracing::debug;

use crate::http::benchmarks_at;
use crate::types::PriceFeedId;
use crate::vol::{log_returns, realized_vol};

const DAY_SECS: i64 = 86_400;
/// Trading days per year used to annualize the daily-sampled stddev.
const ANNUALIZATION_DAYS: f64 = 365.0;
/// Minimum spacing between Benchmarks requests. The public cap is ~10 req per
/// 10s; one request per ~1.1s stays comfortably under it.
const MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(1_100);
/// How long a failed day fetch is remembered before it's retried. Without
/// this, a day Benchmarks keeps 401/429-ing (observed 2026-09-01: keyed
/// requests rate-limited hard from datacenter IPs) is re-attempted on EVERY
/// sigma call, so each call pays the full paced walk over the same doomed
/// days instead of answering from the closes it already holds.
const FAILED_DAY_COOLDOWN: Duration = Duration::from_secs(300);

/// Shared client for realized-vol lookups against Pyth Benchmarks. Holds the
/// immutable-close cache and the request pacer; construct one and share it
/// (e.g. behind an `Arc`) across all vaults and ticks.
pub struct BenchmarkVol {
    http: reqwest::Client,
    benchmarks_url: String,
    // (feed, day_boundary_secs) -> close price. Daily closes never change, so
    // entries are kept for the process lifetime. A feed absent from a
    // SUCCESSFUL day response is stored as NaN — "no observation exists" is
    // itself immutable and must not be refetched forever.
    cache: Mutex<HashMap<(PriceFeedId, i64), f64>>,
    // day -> when its last fetch failed; skipped until FAILED_DAY_COOLDOWN.
    failed_days: Mutex<HashMap<i64, Instant>>,
    // Earliest instant the next request may start — serializes and paces all
    // requests from this client.
    next_allowed: tokio::sync::Mutex<Option<Instant>>,
}

impl BenchmarkVol {
    pub fn new(http: reqwest::Client, benchmarks_url: impl Into<String>) -> Self {
        Self {
            http,
            benchmarks_url: benchmarks_url.into(),
            cache: Mutex::new(HashMap::new()),
            failed_days: Mutex::new(HashMap::new()),
            next_allowed: tokio::sync::Mutex::new(None),
        }
    }

    /// The `window_days + 1` UTC day boundaries ending at the day containing
    /// `now_secs`, oldest first.
    fn day_boundaries(window_days: u32, now_secs: i64) -> Vec<i64> {
        let today = now_secs - now_secs.rem_euclid(DAY_SECS);
        (0..=window_days as i64)
            .rev()
            .map(|i| today - i * DAY_SECS)
            .collect()
    }

    /// Annualized close-to-close realized vol for a single feed.
    pub async fn realized_sigma(
        &self,
        feed: PriceFeedId,
        window_days: u32,
        now_secs: i64,
    ) -> Result<f64> {
        let mut out = self.realized_sigma_bulk(&[feed], window_days, now_secs).await;
        out.remove(&feed)
            .unwrap_or_else(|| Err(anyhow::anyhow!("no sigma computed for {feed}")))
    }

    /// Annualized realized vol for many feeds at once. Fills the cache one
    /// bulk request per day (covering all feeds that still need that day),
    /// then computes each feed's sigma from the cache. Returns a per-feed
    /// result so one feed's gap doesn't sink the others.
    pub async fn realized_sigma_bulk(
        &self,
        feeds: &[PriceFeedId],
        window_days: u32,
        now_secs: i64,
    ) -> HashMap<PriceFeedId, Result<f64>> {
        let boundaries = Self::day_boundaries(window_days, now_secs);

        // Fill any missing (feed, day) cells, one paced bulk request per day.
        for &day in &boundaries {
            let missing: Vec<PriceFeedId> = {
                let cache = self.cache.lock().unwrap();
                feeds
                    .iter()
                    .copied()
                    .filter(|f| !cache.contains_key(&(*f, day)))
                    .collect()
            };
            if missing.is_empty() {
                continue;
            }
            // A day that failed recently is skipped, not retried — see
            // FAILED_DAY_COOLDOWN. The sigma below just uses a shorter series.
            {
                let failed = self.failed_days.lock().unwrap();
                if let Some(at) = failed.get(&day) {
                    if at.elapsed() < FAILED_DAY_COOLDOWN {
                        continue;
                    }
                }
            }
            match self.fetch_day(&missing, day).await {
                Ok(found) => {
                    let mut cache = self.cache.lock().unwrap();
                    for (feed, price) in &found {
                        cache.insert((*feed, day), *price);
                    }
                    // Feeds the (successful) response did not cover have no
                    // observation for this day — tombstone them so they are
                    // never refetched. NaN is filtered out of the closes.
                    let served: std::collections::HashSet<PriceFeedId> =
                        found.iter().map(|(f, _)| *f).collect();
                    for feed in missing.iter().filter(|f| !served.contains(f)) {
                        cache.insert((*feed, day), f64::NAN);
                    }
                    self.failed_days.lock().unwrap().remove(&day);
                }
                Err(e) => {
                    debug!(day, error = %format!("{e:#}"), "benchmark day fetch failed");
                    self.failed_days.lock().unwrap().insert(day, Instant::now());
                }
            }
        }

        // Compute each feed's sigma from whatever the cache now holds.
        let cache = self.cache.lock().unwrap();
        feeds
            .iter()
            .copied()
            .map(|feed| {
                let closes: Vec<f64> = boundaries
                    .iter()
                    .filter_map(|day| cache.get(&(feed, *day)).copied())
                    .filter(|close| close.is_finite())
                    .collect();
                (feed, sigma_from_closes(feed, &closes))
            })
            .collect()
    }

    /// One paced, bulk Benchmarks request for `feeds` at `day`. Returns the
    /// `(feed, close)` pairs the endpoint served (feeds with no observation
    /// are simply absent).
    async fn fetch_day(&self, feeds: &[PriceFeedId], day: i64) -> Result<Vec<(PriceFeedId, f64)>> {
        // Serialize + space requests: hold the pacer across the request so two
        // callers can't burst, and don't start before `next_allowed`.
        let mut next = self.next_allowed.lock().await;
        if let Some(t) = *next {
            let now = Instant::now();
            if t > now {
                tokio::time::sleep(t - now).await;
            }
        }
        let result = benchmarks_at(&self.http, &self.benchmarks_url, feeds, day).await;
        *next = Some(Instant::now() + MIN_REQUEST_INTERVAL);
        drop(next);

        let updates = result?;
        let mut out = Vec::with_capacity(updates.len());
        for upd in updates {
            let feed = match upd.feed_id() {
                Ok(f) => f,
                Err(e) => {
                    debug!(error = %e, "benchmark response had unparseable feed id");
                    continue;
                }
            };
            match upd.price.price_f64() {
                Ok(p) if p.is_finite() && p > 0.0 => out.push((feed, p)),
                Ok(p) => debug!(%feed, price = p, "skipping non-positive benchmark close"),
                Err(e) => debug!(%feed, error = %e, "decoding benchmark close"),
            }
        }
        Ok(out)
    }
}

/// Annualized realized vol from an ordered (oldest-first) close series.
fn sigma_from_closes(feed: PriceFeedId, closes: &[f64]) -> Result<f64> {
    let rets = log_returns(closes);
    if rets.len() < 2 {
        bail!(
            "not enough benchmark samples for {feed}: {} closes",
            closes.len()
        );
    }
    let sigma = realized_vol(&rets, ANNUALIZATION_DAYS);
    if !sigma.is_finite() || sigma <= 0.0 {
        bail!("degenerate realized vol {sigma} for {feed}");
    }
    debug!(%feed, sigma, closes = closes.len(), "realized vol from benchmarks (cached)");
    Ok(sigma)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_boundaries_are_aligned_and_inclusive() {
        // Mid-day now → boundaries snap to UTC midnight and span window+1 days.
        let now = 1_700_000_123; // arbitrary mid-day timestamp
        let b = BenchmarkVol::day_boundaries(3, now);
        assert_eq!(b.len(), 4);
        for w in b.windows(2) {
            assert_eq!(w[1] - w[0], DAY_SECS);
        }
        for ts in &b {
            assert_eq!(ts.rem_euclid(DAY_SECS), 0, "{ts} not a day boundary");
        }
        // Newest boundary is the midnight of the day containing `now`.
        assert_eq!(*b.last().unwrap(), now - now.rem_euclid(DAY_SECS));
    }

    #[test]
    fn sigma_needs_enough_closes() {
        let feed = PriceFeedId::from_hex(
            "e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43",
        )
        .unwrap();
        assert!(sigma_from_closes(feed, &[100.0]).is_err());
        assert!(sigma_from_closes(feed, &[100.0, 101.0]).is_err());
        assert!(sigma_from_closes(feed, &[100.0, 101.0, 102.0]).is_ok());
    }
}
