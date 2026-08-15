//! Realized-vol estimate from Pyth Benchmarks daily closes — annualized
//! σ over a trailing window, one sample per day. Shared by the
//! api-service's `/buckets` ladder and the vault-keeper's IV estimate
//! (doc 05 §4.2 / doc 04 §9); call volume is one request per day of the
//! window per roll, so the public endpoint's rate cap is plenty.

use anyhow::{bail, Context, Result};
use tracing::debug;

use crate::http::benchmark_at;
use crate::types::PriceFeedId;
use crate::vol::{log_returns, realized_vol};

const DAY_SECS: i64 = 86_400;

/// Annualized close-to-close realized vol over the trailing
/// `window_days`, sampled at daily boundaries ending now.
pub async fn realized_sigma_from_benchmarks(
    client: &reqwest::Client,
    benchmarks_url: &str,
    feed: PriceFeedId,
    window_days: u32,
    now_secs: i64,
) -> Result<f64> {
    let mut closes: Vec<f64> = Vec::with_capacity(window_days as usize + 1);
    for i in (0..=window_days as i64).rev() {
        let t = now_secs - i * DAY_SECS;
        let update = benchmark_at(client, benchmarks_url, feed, t)
            .await
            .with_context(|| format!("fetching benchmark for {feed} @ {t}"))?;
        let price = update.price.price_f64().context("decoding benchmark price")?;
        if !price.is_finite() || price <= 0.0 {
            bail!("non-positive benchmark price {price} for {feed} @ {t}");
        }
        closes.push(price);
    }
    let rets = log_returns(&closes);
    if rets.len() < 2 {
        bail!(
            "not enough benchmark samples for {feed}: {} closes",
            closes.len()
        );
    }
    let sigma = realized_vol(&rets, 365.0);
    debug!(%feed, window_days, sigma, "realized vol from benchmarks");
    if !sigma.is_finite() || sigma <= 0.0 {
        bail!("degenerate realized vol {sigma} for {feed}");
    }
    Ok(sigma)
}
