//! Crossbar-backed price data plane (SO-353).
//!
//! Pyth's hermes-beta network stopped publishing on 2026-08-04, starving
//! the WS fanout every consumer's staleness gate keys off — mm-bot
//! declined every RFQ for 41 hours. The SO-335 seam had deliberately kept
//! the streaming data plane on Hermes ("the only live streaming source");
//! this module is the Switchboard half it lacked: when
//! `[oracle] provider = "switchboard"`, prices come from our crossbar's
//! `GET /v2/simulate` — UNSIGNED reads of its live Surge exchange stream,
//! independent of the signing-oracle cache `/v2/update` needs.
//!
//! ## Keyed by the PYTH feed id, on purpose
//!
//! Every WS/cache consumer (mm-bot, market-sim, api-service, keeper,
//! price-charting) resolves its subscription keys from the catalog's
//! `pyth_feed_id`. Publishing under each token's pyth id — via the
//! `switchboard_hash → pyth_feed_id` alias map built at boot — means zero
//! consumer changes and no flag-day deploy. The honest re-key (consumers
//! resolving `feed_for(provider)` from the descriptor) is follow-up work;
//! until then the pyth id is a cache KEY, not a Pyth dependency.
//!
//! ## Failure posture
//!
//! A failed poll publishes nothing: consumers' staleness gates (5s/10s in
//! mm-bot) are the designed response to a dark upstream. After
//! [`DISCONNECT_AFTER_FAILURES`] consecutive failures the poller emits
//! `Disconnected` so `fanout::run` drops `upstream_healthy` (surfaced on
//! `/health` and the WS status message), and `Reconnected` on recovery.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pyth_client::{PriceFeedId, PriceUpdate, PythPrice, StreamEvent};
use switchboard_client::{CrossbarClient, SimulatedPrice};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Poll cadence. Consumers gate at `max_price_age` 5s / `max_publish_lag`
/// 10s (mm-bot defaults), so 1.5s keeps several polls inside every
/// window without hammering crossbar.
const POLL_INTERVAL: Duration = Duration::from_millis(1500);

/// Consecutive whole-poll failures before the fanout is told the
/// upstream is down. One flaky request shouldn't flap `/health`.
const DISCONNECT_AFTER_FAILURES: u32 = 3;

/// Synthesized updates use this exponent: value × 10⁻⁸ survives the
/// `PythPrice` stringified-i64 wire shape with sub-cent precision for
/// anything from WAL ($0.025) to BTC, and `i64::MAX × 10⁻⁸ ≈ 9.2e10`
/// leaves headroom over any plausible USD price.
const SYNTH_EXPO: i32 = -8;

/// Spawn the poller. Returns the same channel shape as
/// `pyth_client::spawn_subscriber`, so `fanout::run` consumes either
/// upstream unchanged — cache inserts and WS broadcasts stay in the one
/// place that keeps them from diverging.
///
/// `alias`: `switchboard feed hash (lowercase hex, no 0x) → the feed id
/// to publish under`.
pub fn spawn_crossbar_poller(
    crossbar: CrossbarClient,
    alias: BTreeMap<String, PriceFeedId>,
    ids: usize,
) -> mpsc::Receiver<StreamEvent> {
    let (tx, rx) = mpsc::channel(64);
    info!(
        feeds = alias.len(),
        catalog_ids = ids,
        poll_ms = POLL_INTERVAL.as_millis() as u64,
        "crossbar simulate poller starting (switchboard data plane)"
    );
    tokio::spawn(run_loop(crossbar, alias, tx));
    rx
}

async fn run_loop(
    crossbar: CrossbarClient,
    alias: BTreeMap<String, PriceFeedId>,
    tx: mpsc::Sender<StreamEvent>,
) {
    let hashes: Vec<String> = alias.keys().cloned().collect();
    let mut consecutive_failures: u32 = 0;
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        match crossbar.simulate(&hashes).await {
            Ok(prices) => {
                if consecutive_failures >= DISCONNECT_AFTER_FAILURES
                    && tx.send(StreamEvent::Reconnected).await.is_err()
                {
                    return; // receiver dropped — service shutting down
                }
                consecutive_failures = 0;
                let now_s = unix_seconds();
                for upd in updates_from_simulation(&prices, &alias, now_s) {
                    if tx.send(StreamEvent::Price(upd)).await.is_err() {
                        return;
                    }
                }
            }
            Err(e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                warn!(
                    attempt = consecutive_failures,
                    error = %format!("{e:#}"),
                    "crossbar simulate poll failed"
                );
                if consecutive_failures == DISCONNECT_AFTER_FAILURES
                    && tx
                        .send(StreamEvent::Disconnected {
                            reason: format!("{e:#}"),
                        })
                        .await
                        .is_err()
                {
                    return;
                }
            }
        }
    }
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Map one poll's results into fanout-ready `PriceUpdate`s.
///
/// `publish_time` is the poll instant: simulate reads crossbar's live
/// Surge cache and carries no upstream timestamp. If crossbar's own
/// stream stalls, its `/v2/simulate` keeps answering with the last value
/// under a fresh stamp — the residual risk the SO-354 watchdog and
/// crossbar's own health monitoring carry, since this service cannot see
/// through the endpoint.
fn updates_from_simulation(
    prices: &[SimulatedPrice],
    alias: &BTreeMap<String, PriceFeedId>,
    publish_time: i64,
) -> Vec<PriceUpdate> {
    prices
        .iter()
        .filter_map(|p| {
            let Some(feed_id) = alias.get(&p.feed_hash) else {
                warn!(feed_hash = %p.feed_hash, "simulate returned a hash outside the alias map; dropping");
                return None;
            };
            let scaled = p.value * 10f64.powi(-SYNTH_EXPO);
            if !scaled.is_finite() || scaled <= 0.0 || scaled >= i64::MAX as f64 {
                warn!(feed_hash = %p.feed_hash, value = p.value, "simulated price out of range; dropping");
                return None;
            }
            Some(PriceUpdate {
                id: feed_id.to_hex(),
                price: PythPrice {
                    price: (scaled.round() as i64).to_string(),
                    conf: "0".to_string(),
                    expo: SYNTH_EXPO,
                    publish_time,
                },
                ema_price: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(byte: u8) -> PriceFeedId {
        PriceFeedId([byte; 32])
    }

    fn alias_of(pairs: &[(&str, PriceFeedId)]) -> BTreeMap<String, PriceFeedId> {
        pairs.iter().map(|(h, id)| (h.to_string(), *id)).collect()
    }

    #[test]
    fn simulated_value_round_trips_through_the_pyth_wire_shape() {
        let alias = alias_of(&[("aa".into(), feed(1))]);
        let prices = vec![SimulatedPrice {
            feed_hash: "aa".into(),
            value: 64_477.6,
        }];
        let upds = updates_from_simulation(&prices, &alias, 1_700_000_000);
        assert_eq!(upds.len(), 1);
        assert_eq!(upds[0].id, feed(1).to_hex());
        assert_eq!(upds[0].price.publish_time, 1_700_000_000);
        // The consumer-side parse must recover the simulated value.
        let back = upds[0].price.price_f64().unwrap();
        assert!((back - 64_477.6).abs() < 1e-6, "got {back}");
        assert_eq!(upds[0].price.conf_f64().unwrap(), 0.0);
    }

    #[test]
    fn sub_dollar_prices_keep_precision() {
        // WAL at $0.02538 — the magnitude that motivated SYNTH_EXPO=-8.
        let alias = alias_of(&[("aa".into(), feed(1))]);
        let prices = vec![SimulatedPrice {
            feed_hash: "aa".into(),
            value: 0.02538,
        }];
        let upds = updates_from_simulation(&prices, &alias, 0);
        let back = upds[0].price.price_f64().unwrap();
        assert!((back - 0.02538).abs() < 1e-9, "got {back}");
    }

    #[test]
    fn unknown_hashes_and_junk_values_are_dropped() {
        let alias = alias_of(&[("aa".into(), feed(1))]);
        let prices = vec![
            SimulatedPrice {
                feed_hash: "zz".into(),
                value: 1.0,
            }, // not in map
            SimulatedPrice {
                feed_hash: "aa".into(),
                value: 0.0,
            }, // non-positive
            SimulatedPrice {
                feed_hash: "aa".into(),
                value: -3.0,
            },
            SimulatedPrice {
                feed_hash: "aa".into(),
                value: f64::INFINITY,
            },
            SimulatedPrice {
                feed_hash: "aa".into(),
                value: 1e12,
            }, // overflows i64 at 1e8 scale
        ];
        assert!(updates_from_simulation(&prices, &alias, 0).is_empty());
    }
}
