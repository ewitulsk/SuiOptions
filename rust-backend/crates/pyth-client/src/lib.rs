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

pub mod cache;
pub mod http;
pub mod stream;
pub mod types;
pub mod vol;

pub use cache::{CachedPrice, PriceCache};
pub use http::{benchmark_at, latest};
pub use stream::{spawn_subscriber, StreamEvent};
pub use types::{HermesEnvelope, PriceFeedId, PriceUpdate, PythPrice};
pub use vol::{log_returns, realized_vol, RollingVolBuffer};
