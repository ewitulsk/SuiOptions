//! Pyth Network Hermes client.
//!
//! Three pieces, kept deliberately small and independent:
//!   - [`http`] — one-shot `GET /v2/updates/price/latest` and
//!     `GET /v1/updates/price/{ts}` (Benchmarks).
//!   - [`stream`] — SSE subscriber for `/v2/updates/price/stream`. Owns
//!     a tokio task that reconnects on failure.
//!   - [`cache`] — `PriceCache` that the stream task writes into and
//!     hot-path code reads from.
//!   - [`vol`] — realized-vol math + `RollingVolBuffer`.
//!
//! Typical wiring (see `services/mm-bot/src/main.rs` for the full thing):
//!
//! ```ignore
//! let client = reqwest::Client::builder().build()?;
//! let cache = pyth::PriceCache::new();
//! let rx = pyth::spawn_subscriber(client.clone(), hermes_url, vec![btc, usdc]);
//! cache.spawn_updater(rx);
//! // …RFQ hot path…
//! let p = cache.get_fresh(btc, Duration::from_secs(5), Duration::from_secs(10))?;
//! ```

pub mod benchmark;
pub mod benchmark_cache;
pub mod cache;
pub mod http;
pub mod sigma;
pub mod stream;
pub mod types;
pub mod vol;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};

/// Default headers for an authenticated Pyth client.
///
/// Attach the result via [`reqwest::ClientBuilder::default_headers`] so every
/// Hermes and Benchmarks request (one-shot and the SSE stream) carries
/// `Authorization: Bearer <api_key>`. Pyth requires this for Core access from
/// 2026-07-31 and it lifts the 10-req/10s anonymous per-IP rate limit.
///
/// `api_key = None` (or an unrepresentable value) yields an empty map, leaving
/// the client on the anonymous tier.
pub fn auth_headers(api_key: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        if let Ok(mut value) = HeaderValue::from_str(&format!("Bearer {key}")) {
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
    }
    headers
}

pub use benchmark::benchmark_feed_id;
pub use benchmark_cache::BenchmarkVol;
pub use cache::{CachedPrice, PriceCache};
pub use http::{benchmark_at, benchmarks_at, latest, latest_with_update_data};
pub use stream::{spawn_subscriber, StreamEvent};
pub use types::{HermesEnvelope, PriceFeedId, PriceUpdate, PythPrice};
pub use vol::{log_returns, realized_vol, realized_vol_with, RollingVolBuffer, VolEstimator};
