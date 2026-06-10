//! DeepBook quoting loop (SO-158).
//!
//! Discovers tradeable bucket pools from api-service, prices each bucket's
//! option with the SAME Black-Scholes path the RFQ loop uses (one fair-value
//! implementation — `pricing::price_rfq` — with the existing ask-markup /
//! bid-markdown knobs giving the two sides), and rests POST-only limit
//! orders through the bot's DeepBook BalanceManager.
//!
//! Multi-market (SO-159): the quoter mirrors the RFQ loop's `Market` model —
//! one entry per configured underlying, each with its own Pyth feed and vol
//! buffer, all against the shared settlement. A tradeable bucket is matched
//! to its market by underlying coin type; pools outside the configured
//! markets are left alone.
//!
//! Inventory model: funds live in the BM. Each refresh sweeps the wallet's
//! call coins into the BM (the bot accrues them organically when it serves
//! as Trader MM) and tops up TUSDC from the wallet when the BM runs short.
//! Quantities clamp to what the BM holds, so one-sided quoting is the
//! expected state for a fresh bucket.
//!
//! Price units: `pricing::price_rfq` returns `per_unit` in settlement-atomic
//! per underlying-atomic — exactly DeepBook's price ratio — so the DeepBook
//! raw price is `per_unit × 10^9`, rounded to tick (up for asks, down for
//! bids; rounding always moves away from mid).
//!
//! Every submit inherits the dry-run gate in `sui_tx::tx::deepbook`; a
//! refused refresh leaves the previous orders standing until their on-chain
//! expiry. On Ctrl-C the task cancels everything it quoted before exiting.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use serde::Deserialize;
use sui_types::base_types::ObjectID;

use api_service_client::{ApiServiceClient, TradeableBucket};
use protocol_types::sides::Side;
use pyth_client::{PriceCache, PriceFeedId, RollingVolBuffer};
use sui_tx::sui_client::{Network, SuiClientWrapper};
use sui_tx::tx::deepbook::{
    bm_balance, cancel_all_on_pool, create_balance_manager, find_balance_manager,
    refresh_pool_quotes, DeepBookHandles, QuotePlan, QuoteSide,
};

use crate::pricing::{
    compute_spot_from_cache, price_rfq, resolve_sigma, PriceDecision, PricingConfig,
    RfqPricingInputs, Staleness,
};

// -- Config ----------------------------------------------------------------

fn default_quote_interval_secs() -> u64 {
    30
}
fn default_max_quote_age_secs() -> u64 {
    120
}
fn default_requote_drift_bps() -> u64 {
    50
}
fn default_expiry_cutoff_secs() -> u64 {
    3_600
}
fn default_order_lifetime_secs() -> u64 {
    600
}
fn default_quote_size() -> u64 {
    1_000_000
}
fn default_tusdc_deposit_chunk() -> u64 {
    1_000_000_000 // 1,000 TUSDC (6 dec)
}

/// `[deepbook]` section of the bot config. Disabled by default — flipping
/// `enabled = true` requires the network to have a DeepBook deployment in
/// token-info.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DeepBookQuoterConfig {
    pub enabled: bool,
    /// Known BalanceManager id. Leave unset on first run: the bot finds its
    /// registered BM on-chain (BalanceManagerEvent by owner) or creates one
    /// and logs the id — paste it here to skip the scan on later boots.
    pub balance_manager_id: Option<String>,
    /// Re-quote cadence; also the pool-discovery cadence.
    pub quote_interval_secs: u64,
    /// Re-quote even without drift once quotes are this old.
    pub max_quote_age_secs: u64,
    /// Re-quote when the fair mid moves this far from the quoted mid.
    pub requote_drift_bps: u64,
    /// Stop quoting (and cancel) this close to bucket expiry — passive
    /// spreads near expiry are free money for snipers.
    pub expiry_cutoff_secs: u64,
    /// On-chain order expiry; stale quotes die even if the bot is gone.
    pub order_lifetime_secs: u64,
    /// Per-side order size in base (call-coin) atomic units, clamped by
    /// inventory and rounded down to the pool's lot.
    pub quote_size: u64,
    /// Top the BM's settlement balance up from the wallet in chunks of this
    /// many atomic units whenever it can't fund the bid.
    pub tusdc_deposit_chunk: u64,
    pub gas_budget: u64,
}

impl Default for DeepBookQuoterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            balance_manager_id: None,
            quote_interval_secs: default_quote_interval_secs(),
            max_quote_age_secs: default_max_quote_age_secs(),
            requote_drift_bps: default_requote_drift_bps(),
            expiry_cutoff_secs: default_expiry_cutoff_secs(),
            order_lifetime_secs: default_order_lifetime_secs(),
            quote_size: default_quote_size(),
            tusdc_deposit_chunk: default_tusdc_deposit_chunk(),
            gas_budget: 200_000_000,
        }
    }
}

// -- Quoter ------------------------------------------------------------------

/// One underlying the quoter makes DeepBook markets in — the deepbook-side
/// mirror of main.rs's `Market` (same feed + shared vol buffer).
pub struct QuoterMarket {
    pub symbol: String,
    /// Canonical underlying coin type; buckets match on this.
    pub coin_type: String,
    pub feed: PriceFeedId,
    pub decimals: u8,
    pub vol_buf: Arc<RwLock<RollingVolBuffer>>,
}

/// Everything the quoter task needs, captured at boot.
pub struct QuoterParams {
    pub cfg: DeepBookQuoterConfig,
    pub secrets: runtime_config::Secrets,
    pub network: Network,
    pub handles: DeepBookHandles,
    pub api_url: String,
    pub price_cache: PriceCache,
    /// One per configured underlying (SO-159), shared settlement.
    pub markets: Vec<QuoterMarket>,
    pub settlement_feed: PriceFeedId,
    pub settlement_coin_type: String,
    pub settlement_decimals: u8,
    pub pricing: PricingConfig,
    pub staleness: Staleness,
    pub fallback_vol: f64,
}

#[derive(Debug, Clone, Copy)]
struct LastQuote {
    mid_raw: u64,
    at_ms: u64,
}

/// Pool sizing the quoter rounds against. Our venues are created by the
/// frontend's `deriveVenueParams` (SO-154); this mirrors that formula. A
/// pool created with different params makes a refresh fail its dry run
/// (price off tick), which logs loudly and leaves the book untouched.
fn derived_pool_params(base_decimals: u8, quote_decimals: u8) -> (u64, u64, u64) {
    let price_exp = 9i32 - base_decimals as i32 + quote_decimals as i32;
    let tick = 10u64.pow((price_exp - 2).max(0) as u32);
    let lot = 10u64.pow((base_decimals as i32 - 5).max(3) as u32);
    let min = 10 * lot;
    (tick, lot, min)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn spawn_quoter(p: QuoterParams) {
    tokio::spawn(async move {
        if let Err(e) = run(p).await {
            tracing::error!(error = %format!("{e:#}"), "deepbook quoter exited");
        }
    });
}

async fn run(p: QuoterParams) -> anyhow::Result<()> {
    let wrap = SuiClientWrapper::connect(&p.secrets, p.network).await?;
    let api = ApiServiceClient::new(&p.api_url);

    // Resolve the BM: config pin → on-chain scan → create.
    let bm_id = match &p.cfg.balance_manager_id {
        Some(s) => ObjectID::from_hex_literal(s)
            .map_err(|e| anyhow::anyhow!("bad deepbook.balance_manager_id {s}: {e}"))?,
        None => match find_balance_manager(&wrap.client, &p.handles, wrap.signer.address).await? {
            Some(id) => {
                tracing::info!(balance_manager = %id, "recovered registered BalanceManager");
                id
            }
            None => {
                let id =
                    create_balance_manager(&wrap.client, &wrap.signer, &p.handles, p.cfg.gas_budget)
                        .await?;
                tracing::info!(
                    balance_manager = %id,
                    "created BalanceManager — pin it as [deepbook].balance_manager_id to skip the scan"
                );
                id
            }
        },
    };

    let mut last: HashMap<String, LastQuote> = HashMap::new();
    // pool_id → (base, quote) types, for the shutdown cancel sweep.
    let mut quoted_pools: HashMap<String, (String, String)> = HashMap::new();

    let mut ticker =
        tokio::time::interval(Duration::from_secs(p.cfg.quote_interval_secs.max(5)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Err(e) = cycle(&p, &wrap, &api, bm_id, &mut last, &mut quoted_pools).await {
                    tracing::warn!(error = %format!("{e:#}"), "deepbook quote cycle failed; retrying next tick");
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!(pools = quoted_pools.len(), "shutdown: cancelling deepbook quotes");
                for (pool, (base, quote)) in &quoted_pools {
                    if let Ok(pool_id) = ObjectID::from_hex_literal(pool) {
                        if let Err(e) = cancel_all_on_pool(
                            &wrap.client, &wrap.signer, &p.handles,
                            pool_id, base, quote, bm_id, p.cfg.gas_budget,
                        ).await {
                            tracing::warn!(pool = %pool, error = %format!("{e:#}"), "shutdown cancel failed");
                        }
                    }
                }
                return Ok(());
            }
        }
    }
}

async fn cycle(
    p: &QuoterParams,
    wrap: &SuiClientWrapper,
    api: &ApiServiceClient,
    bm_id: ObjectID,
    last: &mut HashMap<String, LastQuote>,
    quoted_pools: &mut HashMap<String, (String, String)>,
) -> anyhow::Result<()> {
    let buckets = api.tradeable_buckets().await?;
    let now = now_ms();

    // Only pairs we source a Pyth spot for: settlement matches and the
    // underlying is one of the configured markets.
    let ours: Vec<(&TradeableBucket, usize)> = buckets
        .iter()
        .filter(|b| b.settlement_coin_type == p.settlement_coin_type)
        .filter_map(|b| {
            p.markets
                .iter()
                .position(|m| m.coin_type == b.asset_coin_type)
                .map(|i| (b, i))
        })
        .collect();

    // Pools that left the tradeable set (expired / cleaned): cancel + forget.
    let live: std::collections::HashSet<&str> =
        ours.iter().map(|(b, _)| b.pool_id.as_str()).collect();
    let gone: Vec<String> = quoted_pools
        .keys()
        .filter(|k| !live.contains(k.as_str()))
        .cloned()
        .collect();
    for pool in gone {
        if let Some((base, quote)) = quoted_pools.remove(&pool) {
            last.remove(&pool);
            if let Ok(pool_id) = ObjectID::from_hex_literal(&pool) {
                tracing::info!(pool = %pool, "bucket left tradeable set; cancelling quotes");
                if let Err(e) = cancel_all_on_pool(
                    &wrap.client, &wrap.signer, &p.handles,
                    pool_id, &base, &quote, bm_id, p.cfg.gas_budget,
                )
                .await
                {
                    tracing::warn!(pool = %pool, error = %format!("{e:#}"), "exit cancel failed");
                }
            }
        }
    }

    if ours.is_empty() {
        return Ok(());
    }

    // One spot/sigma read per market with buckets this cycle; `None` where
    // that market's feed is currently stale (its pools keep their previous
    // quotes — they self-expire on-chain).
    let mut spots: HashMap<usize, Option<(f64, f64)>> = HashMap::new();
    for (_, mi) in &ours {
        spots.entry(*mi).or_insert_with(|| {
            let m = &p.markets[*mi];
            match compute_spot_from_cache(
                &p.price_cache,
                m.feed,
                p.settlement_feed,
                m.decimals,
                p.settlement_decimals,
                p.staleness,
            ) {
                Ok(spot) => Some((
                    spot,
                    resolve_sigma(m.vol_buf.read().current_annualized(), p.fallback_vol),
                )),
                Err(e) => {
                    tracing::warn!(
                        market = %m.symbol,
                        reason = e.as_str(),
                        "deepbook: no fresh spot; leaving this market's books as-is"
                    );
                    None
                }
            }
        });
    }

    for (b, mi) in ours {
        let Some(Some((spot_scaled, sigma))) = spots.get(&mi) else {
            continue;
        };
        if let Err(e) = quote_one(
            p, wrap, api, bm_id, last, quoted_pools, b, mi, *spot_scaled, *sigma, now,
        )
        .await
        {
            tracing::warn!(
                bucket = %b.bucket_id.to_hex(),
                pool = %b.pool_id,
                error = %format!("{e:#}"),
                "deepbook: refresh failed for pool; continuing"
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn quote_one(
    p: &QuoterParams,
    wrap: &SuiClientWrapper,
    _api: &ApiServiceClient,
    bm_id: ObjectID,
    last: &mut HashMap<String, LastQuote>,
    quoted_pools: &mut HashMap<String, (String, String)>,
    b: &TradeableBucket,
    market_idx: usize,
    spot_scaled: f64,
    sigma: f64,
    now: u64,
) -> anyhow::Result<()> {
    let pool_key = b.pool_id.clone();
    let pool_id = ObjectID::from_hex_literal(&b.pool_id)
        .map_err(|e| anyhow::anyhow!("bad pool id {}: {e}", b.pool_id))?;

    // Too close to expiry: cancel anything resting and stand down.
    let cutoff_ms = p.cfg.expiry_cutoff_secs.saturating_mul(1_000);
    if b.expiry_ms.saturating_sub(now) < cutoff_ms {
        if quoted_pools.remove(&pool_key).is_some() {
            last.remove(&pool_key);
            tracing::info!(pool = %pool_key, "expiry cutoff reached; cancelling quotes");
            cancel_all_on_pool(
                &wrap.client, &wrap.signer, &p.handles,
                pool_id, &b.call_coin_type, &b.settlement_coin_type, bm_id, p.cfg.gas_budget,
            )
            .await?;
        }
        return Ok(());
    }

    // Fair value: same engine, both sides. Side::Trader carries the ask
    // markup, Side::Writer the bid markdown.
    let price_for = |side: Side| -> Option<f64> {
        let inputs = RfqPricingInputs {
            write_amount: p.cfg.quote_size,
            side,
            strike: b.strike_raw,
            strike_scale: b.strike_scale,
            expiry_ms: b.expiry_ms,
        };
        match price_rfq(&p.pricing, &inputs, spot_scaled, sigma, now) {
            PriceDecision::Quote { per_unit, .. } => Some(per_unit),
            PriceDecision::Decline { .. } => None,
        }
    };
    let (Some(ask_unit), Some(bid_unit)) = (price_for(Side::Trader), price_for(Side::Writer))
    else {
        tracing::debug!(pool = %pool_key, "priced to zero; not quoting");
        return Ok(());
    };

    let market = &p.markets[market_idx];
    let base_dec = b.asset_decimals.unwrap_or(market.decimals);
    let quote_dec = b.settlement_decimals.unwrap_or(p.settlement_decimals);
    let (tick, lot, min_size) = derived_pool_params(base_dec, quote_dec);

    // per_unit (quote-atomic per base-atomic) → DeepBook raw price (×1e9),
    // rounded away from mid and onto the tick grid.
    let ask_raw = {
        let raw = (ask_unit * 1e9).ceil() as u64;
        raw.div_ceil(tick).max(1) * tick
    };
    let bid_raw = ((bid_unit * 1e9).floor() as u64 / tick) * tick;
    if bid_raw == 0 {
        tracing::debug!(pool = %pool_key, "bid rounds to zero; not quoting");
        return Ok(());
    }
    let mid_raw = (ask_raw + bid_raw) / 2;

    // Skip if our standing quotes are fresh and the fair hasn't moved.
    if let Some(prev) = last.get(&pool_key) {
        let age_ms = now.saturating_sub(prev.at_ms);
        let drift_bps = (mid_raw.abs_diff(prev.mid_raw) as u128)
            .saturating_mul(10_000)
            .checked_div(prev.mid_raw.max(1) as u128)
            .unwrap_or(u128::MAX);
        if age_ms < p.cfg.max_quote_age_secs.saturating_mul(1_000)
            && drift_bps < p.cfg.requote_drift_bps as u128
        {
            return Ok(());
        }
    }

    // Inventory: BM balances + wallet sweep/top-up plans.
    let bm_base =
        bm_balance(&wrap.client, wrap.signer.address, &p.handles, bm_id, &b.call_coin_type).await?;
    let bm_quote = bm_balance(
        &wrap.client,
        wrap.signer.address,
        &p.handles,
        bm_id,
        &b.settlement_coin_type,
    )
    .await?;

    let mut deposits: Vec<(String, u64)> = Vec::new();
    // Sweep the wallet's call coins — they arrive whenever the bot buys
    // options as the Trader MM and are useless sitting in the wallet.
    let wallet_base: u64 = wrap
        .client
        .coin_read_api()
        .get_balance(wrap.signer.address, Some(b.call_coin_type.clone()))
        .await
        .map(|bal| bal.total_balance.min(u64::MAX as u128) as u64)
        .unwrap_or(0);
    if wallet_base > 0 {
        deposits.push((b.call_coin_type.clone(), wallet_base));
    }
    // Bid funding: top the BM up from the wallet in config-sized chunks.
    let bid_notional = (p.cfg.quote_size as u128 * bid_raw as u128 / 1_000_000_000) as u64;
    if bm_quote < bid_notional {
        let wallet_quote: u64 = wrap
            .client
            .coin_read_api()
            .get_balance(wrap.signer.address, Some(b.settlement_coin_type.clone()))
            .await
            .map(|bal| bal.total_balance.min(u64::MAX as u128) as u64)
            .unwrap_or(0);
        let want = p
            .cfg
            .tusdc_deposit_chunk
            .max(bid_notional.saturating_sub(bm_quote));
        let take = want.min(wallet_quote);
        if take > 0 {
            deposits.push((b.settlement_coin_type.clone(), take));
        }
    }

    // Sizes, clamped to post-deposit inventory and the lot grid.
    let ask_inventory = bm_base.saturating_add(wallet_base);
    let ask_qty = (p.cfg.quote_size.min(ask_inventory) / lot) * lot;
    let quote_after_deposit = bm_quote.saturating_add(
        deposits
            .iter()
            .filter(|(t, _)| *t == b.settlement_coin_type)
            .map(|(_, a)| *a)
            .sum::<u64>(),
    );
    let affordable = ((quote_after_deposit as u128 * 1_000_000_000 / bid_raw.max(1) as u128)
        .min(u64::MAX as u128)) as u64;
    let bid_qty = (p.cfg.quote_size.min(affordable) / lot) * lot;

    let plan = QuotePlan {
        ask: (ask_qty >= min_size).then_some(QuoteSide { price_raw: ask_raw, quantity: ask_qty }),
        bid: (bid_qty >= min_size).then_some(QuoteSide { price_raw: bid_raw, quantity: bid_qty }),
        expire_timestamp_ms: now + p.cfg.order_lifetime_secs.saturating_mul(1_000),
    };

    let had_quotes = quoted_pools.contains_key(&pool_key);
    if plan.bid.is_none() && plan.ask.is_none() {
        if had_quotes {
            quoted_pools.remove(&pool_key);
            last.remove(&pool_key);
            tracing::info!(pool = %pool_key, "no inventory on either side; cancelling quotes");
            cancel_all_on_pool(
                &wrap.client, &wrap.signer, &p.handles,
                pool_id, &b.call_coin_type, &b.settlement_coin_type, bm_id, p.cfg.gas_budget,
            )
            .await?;
        } else {
            tracing::debug!(pool = %pool_key, "no inventory on either side; nothing to quote");
        }
        return Ok(());
    }

    let resp = refresh_pool_quotes(
        &wrap.client,
        &wrap.signer,
        &p.handles,
        pool_id,
        &b.call_coin_type,
        &b.settlement_coin_type,
        bm_id,
        &deposits,
        plan,
        p.cfg.gas_budget,
    )
    .await?;

    tracing::info!(
        pool = %pool_key,
        bucket = %b.bucket_id.to_hex(),
        bid = ?plan.bid.map(|s| (s.price_raw, s.quantity)),
        ask = ?plan.ask.map(|s| (s.price_raw, s.quantity)),
        digest = %resp.digest,
        "deepbook quotes refreshed"
    );
    last.insert(pool_key.clone(), LastQuote { mid_raw, at_ms: now });
    quoted_pools.insert(
        pool_key,
        (b.call_coin_type.clone(), b.settlement_coin_type.clone()),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_params_match_frontend_for_tbtc_tusdc() {
        // 8-dec base / 6-dec quote: price exponent 7 → tick 1e5 ($0.01),
        // lot 1e3, min 1e4 — same numbers `deriveVenueParams` produces.
        assert_eq!(derived_pool_params(8, 6), (100_000, 1_000, 10_000));
        // 9-dec base (TWAL-style): exponent 6 → tick 1e4, lot 1e4, min 1e5.
        assert_eq!(derived_pool_params(9, 6), (10_000, 10_000, 100_000));
    }

    #[test]
    fn tick_rounding_moves_away_from_mid() {
        let tick = 100_000u64;
        let ask_unit = 0.0123456_f64; // quote-atomic per base-atomic
        let raw = (ask_unit * 1e9).ceil() as u64;
        let ask = raw.div_ceil(tick).max(1) * tick;
        assert_eq!(ask, 12_400_000); // rounded UP to tick
        let bid_unit = 0.0123456_f64;
        let bid = (((bid_unit * 1e9).floor() as u64) / tick) * tick;
        assert_eq!(bid, 12_300_000); // rounded DOWN to tick
        assert!(ask >= bid);
    }
}
