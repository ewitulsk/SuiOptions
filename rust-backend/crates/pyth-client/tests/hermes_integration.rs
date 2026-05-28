//! Live Hermes / Benchmarks integration tests.
//!
//! All `#[ignore]` so they don't run on the default `cargo test` path —
//! they hit `hermes.pyth.network` and `benchmarks.pyth.network` over the
//! public network and are subject to Pyth's 10-req-per-10-sec ceiling.
//!
//! Run them explicitly:
//!
//! ```sh
//! cargo test -p pyth-client --test hermes_integration -- --ignored --nocapture
//! ```

use std::time::Duration;

use pyth_client::{benchmark_at, latest, spawn_subscriber, PriceCache, PriceFeedId};

const HERMES: &str = "https://hermes.pyth.network";
const BENCHMARKS: &str = "https://benchmarks.pyth.network";

/// Pyth BTC/USD mainnet feed.
const BTC_USD: &str = "e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43";
/// Pyth USDC/USD mainnet feed.
const USDC_USD: &str = "eaa020c61cc479712813461ce153894a96a6c00b21ed0cfc2798d1f9a9e9c94a";

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest client")
}

fn feed(hex: &str) -> PriceFeedId {
    PriceFeedId::from_hex(hex).expect("known-good feed hex")
}

/// `GET /v2/updates/price/latest` returns a parsed `PriceUpdate` whose
/// values decode to sane numbers for BTC/USD.
#[tokio::test]
#[ignore]
async fn hermes_latest_returns_btc_price() {
    let updates = latest(&client(), HERMES, &[feed(BTC_USD)])
        .await
        .expect("latest call");
    assert_eq!(updates.len(), 1);
    let u = &updates[0];
    let p = u.price.price_f64().expect("price_f64");
    // Range is intentionally wide — this guards against decimal/exponent
    // bugs, not Pyth's mark.
    assert!(p > 1_000.0 && p < 1_000_000.0, "BTC/USD looks off: {p}");
}

/// Same endpoint can resolve multiple feeds in one call.
#[tokio::test]
#[ignore]
async fn hermes_latest_returns_two_feeds() {
    let updates = latest(&client(), HERMES, &[feed(BTC_USD), feed(USDC_USD)])
        .await
        .expect("latest call");
    assert_eq!(updates.len(), 2);
    for u in &updates {
        let _ = u.price.price_f64().expect("price_f64 each");
        assert!(!u.id.is_empty());
    }
}

/// `GET /v1/updates/price/{ts}` returns a historical observation. We ask
/// for ~24 hours ago to stay well inside Benchmarks' retention.
#[tokio::test]
#[ignore]
async fn benchmarks_returns_yesterday_price() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let yesterday = now - 24 * 60 * 60;
    let u = benchmark_at(&client(), BENCHMARKS, feed(BTC_USD), yesterday)
        .await
        .expect("benchmark_at call");
    let p = u.price.price_f64().unwrap();
    assert!(p > 1_000.0 && p < 1_000_000.0, "BTC/USD historical looks off: {p}");
    // Publisher timestamp should be within a few hours of what we asked for.
    let asked_ms = yesterday * 1000;
    let diff = (u.price.publish_time_ms() - asked_ms).abs();
    assert!(diff < 6 * 60 * 60 * 1000, "benchmark timestamp off by {diff}ms");
}

/// SSE subscriber + `PriceCache::spawn_updater`: end-to-end happy path.
/// Waits up to 30s for *both* feeds to land in the cache.
#[tokio::test]
#[ignore]
async fn sse_stream_pumps_into_cache() {
    let rx = spawn_subscriber(
        client(),
        HERMES.to_string(),
        vec![feed(BTC_USD), feed(USDC_USD)],
    );
    let cache = PriceCache::new();
    let _updater = cache.spawn_updater(rx);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if cache.peek(feed(BTC_USD)).is_some() && cache.peek(feed(USDC_USD)).is_some() {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("no SSE prices for both feeds within 30s");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let btc = cache.peek(feed(BTC_USD)).unwrap();
    assert!(btc.price > 1_000.0 && btc.price < 1_000_000.0);
}
