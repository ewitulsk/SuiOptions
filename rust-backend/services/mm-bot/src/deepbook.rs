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
use protocol_types::asset::canonicalize_move_type;
use protocol_types::sides::Side;
use pyth_client::{PriceCache, PriceFeedId, RollingVolBuffer};
use sui_tx::sui_client::{Network, SuiClientWrapper};
use sui_tx::tx::deepbook::{
    bm_balance, cancel_all_on_pool, create_balance_manager, derived_pool_params,
    find_balance_manager, refresh_pools_batched, DeepBookHandles, PoolRefresh, QuotePlan,
    QuoteSide,
};

use crate::liquidity::LiquiditySource;
use crate::pricing::{
    compute_spot_from_cache, price_rfq, resolve_sigma, PriceDecision, PricingConfig,
    RfqPricingInputs, SigmaEstimate, Staleness,
};

// -- Config ----------------------------------------------------------------

fn default_quote_interval_secs() -> u64 {
    86_400 // once a day (SO-173): on-chain quotes are refreshed daily
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
    86_400 // match the refresh cadence: orders self-expire by the next update
}
fn default_quote_size() -> u64 {
    1_000_000
}
fn default_tusdc_deposit_chunk() -> u64 {
    1_000_000_000 // 1,000 TUSDC (6 dec)
}
fn default_max_pools_per_tx() -> usize {
    12 // pack up to this many pools' refreshes into one PTB before chunking
}
fn default_inventory_poll_secs() -> u64 {
    10 // watch the wallet this often for newly-settled call-coin inventory
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
    /// Rest the bot's ENTIRE call-coin inventory (BM + wallet sweep) as the
    /// ask instead of `quote_size`, with the bid sized to the same quantity
    /// (floored at `quote_size` so a fresh book is still two-sided). A change
    /// in inventory forces a re-quote so swept fills get listed promptly.
    pub quote_full_inventory: bool,
    /// Top the BM's settlement balance up from the wallet in chunks of this
    /// many atomic units whenever it can't fund the bid.
    pub tusdc_deposit_chunk: u64,
    /// Pull settlement (TUSDC) from the configured liquidity source before
    /// quoting so the bids can mirror the asks. Disable for a market maker
    /// that pre-funds its wallet out-of-band.
    pub settlement_topup_enabled: bool,
    /// Maker-fee + rounding headroom reserved on every resting order, in bps.
    /// DeepBook withdraws `size + maker_fee` from the BalanceManager when an
    /// order rests (the fee is charged in the *input* coin — base for an ask,
    /// quote for a bid — because we place with `pay_with_deep=false` on
    /// non-whitelisted pools), so funding the BM with exactly `size` aborts
    /// `EBalanceManagerBalanceTooLow`. Applied two ways: the ask is sized to
    /// leave this fraction of inventory unrested in the BM, and the bid's BM
    /// deposit (and the wallet top-up that sources it) is grossed up by it.
    pub fee_headroom_bps: u64,
    /// Max pools packed into one refresh PTB before spilling into another tx
    /// ("all prices at once, or until we hit max tx size").
    pub max_pools_per_tx: usize,
    /// How often to poll the wallet for newly-settled call-coin inventory and
    /// re-quote the affected buckets right away (the keeper settles won RFQ
    /// auctions asynchronously, so inventory arrives off the daily cadence).
    /// `0` disables the watcher — quotes refresh only on the daily tick.
    pub inventory_poll_secs: u64,
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
            quote_full_inventory: false,
            tusdc_deposit_chunk: default_tusdc_deposit_chunk(),
            settlement_topup_enabled: true,
            fee_headroom_bps: 100,
            max_pools_per_tx: default_max_pools_per_tx(),
            inventory_poll_secs: default_inventory_poll_secs(),
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
    /// Sigma used while `vol_buf` is cold (per-symbol config override).
    pub fallback_vol: f64,
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
    /// Pulls settlement (and, via the same trait, any coin the bot needs)
    /// before quoting. Defaults to the test-token faucet; a real market maker
    /// swaps in their own funding source.
    pub liquidity: Arc<dyn LiquiditySource>,
}

#[derive(Debug, Clone, Copy)]
struct LastQuote {
    mid_raw: u64,
    /// Ask size targeted at quote time — in full-inventory mode a change
    /// here (new inventory) forces a re-quote even without drift.
    ask_target: u64,
    at_ms: u64,
}

/// What kicked off a quote cycle, deciding which pools it re-quotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger {
    /// The daily ticker — reprice every pool (full refresh).
    Scheduled,
    /// Newly-settled inventory landed in the wallet — re-quote only the
    /// bucket(s) whose inventory changed, leaving the rest on the daily price.
    InventoryArrival,
}

/// A pool's priced quote, captured in the planning pass. The ask is already
/// inventory-sized; the bid is sized against the shared settlement budget in
/// the placement pass, after the liquidity top-up has run.
struct PoolPlan {
    pool_id: ObjectID,
    pool_key: String,
    base_coin_type: String,
    quote_coin_type: String,
    ask_raw: u64,
    bid_raw: u64,
    ask_qty: u64,
    bid_target: u64,
    min_size: u64,
    lot: u64,
    mid_raw: u64,
    ask_target: u64,
}

/// Per-side size targets for one refresh. Fixed `quote_size` by default; in
/// full-inventory mode the ask targets the whole call-coin inventory and the
/// bid mirrors it ("pair it with the equivalent settlement"), floored at
/// `quote_size` so an empty-inventory book still bids.
fn size_targets(cfg: &DeepBookQuoterConfig, call_inventory: u64) -> (u64, u64) {
    if cfg.quote_full_inventory {
        (call_inventory, call_inventory.max(cfg.quote_size))
    } else {
        (cfg.quote_size, cfg.quote_size)
    }
}

/// The most of `inventory` we can rest while leaving `bps` of it in the BM as
/// maker-fee headroom. DeepBook withdraws `size + maker_fee` (fee in the input
/// coin under `pay_with_deep=false`), so resting the full balance leaves
/// nothing for the fee and aborts `EBalanceManagerBalanceTooLow`.
fn reserve_fee_headroom(inventory: u64, bps: u64) -> u64 {
    let keep = 10_000u128.saturating_sub(bps as u128);
    ((inventory as u128).saturating_mul(keep) / 10_000) as u64
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

    // Watch the wallet for newly-settled inventory between daily refreshes.
    let inv_enabled = p.cfg.inventory_poll_secs > 0;
    let mut inv_ticker =
        tokio::time::interval(Duration::from_secs(p.cfg.inventory_poll_secs.max(1)));
    inv_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Wallet call-coin total at the last poll; a rise means fresh inventory.
    let mut last_wallet_inv: u64 = 0;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Err(e) = cycle(&p, &wrap, &api, bm_id, &mut last, &mut quoted_pools, Trigger::Scheduled).await {
                    tracing::warn!(error = %format!("{e:#}"), "deepbook quote cycle failed; retrying next tick");
                }
                // The daily cycle sweeps the wallet into the BM, so reset the
                // baseline; the next poll re-reads what's actually there.
                last_wallet_inv = 0;
            }
            _ = inv_ticker.tick(), if inv_enabled => {
                match wallet_call_inventory(&p, &wrap, &api).await {
                    Ok(cur) => {
                        if cur > last_wallet_inv {
                            tracing::info!(inventory = cur, "new wallet inventory; re-quoting affected pools");
                            if let Err(e) = cycle(&p, &wrap, &api, bm_id, &mut last, &mut quoted_pools, Trigger::InventoryArrival).await {
                                tracing::warn!(error = %format!("{e:#}"), "inventory re-quote failed; retrying next tick");
                            }
                        }
                        // Track the reading either way: a successful sweep drops
                        // `cur` below this, so we don't re-fire; un-sweepable
                        // residual stays equal, so we don't spin.
                        last_wallet_inv = cur;
                    }
                    Err(e) => tracing::debug!(error = %format!("{e:#}"), "inventory poll failed; next tick"),
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

/// Total wallet balance of the bot's call coins across the buckets it makes
/// markets in. A rise between polls means freshly-settled inventory has landed
/// (the keeper settles won auctions into the wallet) and should be listed.
async fn wallet_call_inventory(
    p: &QuoterParams,
    wrap: &SuiClientWrapper,
    api: &ApiServiceClient,
) -> anyhow::Result<u64> {
    let buckets = api.tradeable_buckets().await?;
    // Canonical call-coin types for the pairs we quote (same predicate as `cycle`).
    let ours: std::collections::HashSet<String> = buckets
        .iter()
        .filter(|b| b.settlement_coin_type == p.settlement_coin_type)
        .filter(|b| p.markets.iter().any(|m| m.coin_type == b.asset_coin_type))
        .map(|b| canonicalize_move_type(&b.call_coin_type))
        .collect();
    if ours.is_empty() {
        return Ok(0);
    }
    let total: u128 = wrap
        .client
        .coin_read_api()
        .get_all_balances(wrap.signer.address)
        .await?
        .into_iter()
        .filter(|bal| ours.contains(&canonicalize_move_type(&bal.coin_type)))
        .map(|bal| bal.total_balance)
        .sum();
    Ok(total.min(u64::MAX as u128) as u64)
}

async fn cycle(
    p: &QuoterParams,
    wrap: &SuiClientWrapper,
    api: &ApiServiceClient,
    bm_id: ObjectID,
    last: &mut HashMap<String, LastQuote>,
    quoted_pools: &mut HashMap<String, (String, String)>,
    trigger: Trigger,
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
    let mut spots: HashMap<usize, Option<(f64, SigmaEstimate)>> = HashMap::new();
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
                    resolve_sigma(m.vol_buf.read().current_annualized(), m.fallback_vol),
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

    // Planning pass (SO-173): price every pool first, then place them all in
    // one (or a few) batched txs instead of one tx per pool. A single
    // BalanceManager backs every pool, so the bid (settlement) funding is
    // allocated from one shared budget; call-coin ask inventory is distinct
    // per pool and swept individually. Settlement is sourced between the
    // pricing and placement passes, once the bid demand is known.
    let now_expire = now + p.cfg.order_lifetime_secs.saturating_mul(1_000);

    let mut refreshes: Vec<PoolRefresh> = Vec::new();
    let mut deposits: HashMap<String, u64> = HashMap::new();
    // (pool_key, Some((mid_raw, ask_target, call, settlement)) = placed; None = cancel-only)
    let mut book: Vec<(String, Option<(u64, u64, String, String)>)> = Vec::new();
    let mut plans: Vec<PoolPlan> = Vec::new();
    // Settlement the bot wants to commit across every bid this cycle. In
    // full-inventory mode the bids mirror the asks, so this is the amount of
    // TUSDC to source before placing.
    let mut desired_bid_notional: u128 = 0;

    for (b, mi) in &ours {
        let Some(Some((spot_scaled, sigma))) = spots.get(mi) else {
            continue;
        };
        let pool_key = b.pool_id.clone();
        let pool_id = match ObjectID::from_hex_literal(&b.pool_id) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(pool = %pool_key, error = %e, "bad pool id; skipping");
                continue;
            }
        };

        // Too close to expiry: cancel anything resting, don't re-quote.
        let cutoff_ms = p.cfg.expiry_cutoff_secs.saturating_mul(1_000);
        if b.expiry_ms.saturating_sub(now) < cutoff_ms {
            if quoted_pools.contains_key(&pool_key) {
                refreshes.push(PoolRefresh {
                    pool_id,
                    base_coin_type: b.call_coin_type.clone(),
                    quote_coin_type: b.settlement_coin_type.clone(),
                    plan: QuotePlan { bid: None, ask: None, expire_timestamp_ms: now_expire },
                });
                book.push((pool_key, None));
            }
            continue;
        }

        // Fair value: same engine, both sides.
        let price_for = |side: Side| -> Option<f64> {
            let inputs = RfqPricingInputs {
                write_amount: p.cfg.quote_size,
                side,
                strike: b.strike_raw,
                strike_scale: b.strike_scale,
                expiry_ms: b.expiry_ms,
                is_put: false, // deepbook quoting is call-only
            };
            match price_rfq(&p.pricing, &inputs, *spot_scaled, *sigma, now) {
                PriceDecision::Quote { per_unit, .. } => Some(per_unit),
                PriceDecision::Decline { .. } => None,
            }
        };
        let (Some(ask_unit), Some(bid_unit)) = (price_for(Side::Trader), price_for(Side::Writer))
        else {
            tracing::debug!(pool = %pool_key, "priced to zero; not quoting");
            continue;
        };

        let base_dec = b.asset_decimals.unwrap_or(p.markets[*mi].decimals);
        let quote_dec = b.settlement_decimals.unwrap_or(p.settlement_decimals);
        let (tick, lot, min_size) = derived_pool_params(base_dec, quote_dec);

        let ask_raw = {
            let raw = (ask_unit * 1e9).ceil() as u64;
            raw.div_ceil(tick).max(1) * tick
        };
        let bid_raw = ((bid_unit * 1e9).floor() as u64 / tick) * tick;
        if bid_raw == 0 {
            tracing::debug!(pool = %pool_key, "bid rounds to zero; not quoting");
            continue;
        }
        let mid_raw = (ask_raw + bid_raw) / 2;

        // Call-coin inventory (BM + wallet), distinct per pool. Read before the
        // skip check so a change in inventory (newly swept fills) forces a
        // re-quote in full-inventory mode (SO-161).
        let bm_base = bm_balance(
            &wrap.client,
            wrap.signer.address,
            &p.handles,
            bm_id,
            &b.call_coin_type,
        )
        .await
        .unwrap_or(0);
        let wallet_base: u64 = wrap
            .client
            .coin_read_api()
            .get_balance(wrap.signer.address, Some(b.call_coin_type.clone()))
            .await
            .map(|bal| bal.total_balance.min(u64::MAX as u128) as u64)
            .unwrap_or(0);
        let ask_inventory = bm_base.saturating_add(wallet_base);
        let (ask_target, bid_target) = size_targets(&p.cfg, ask_inventory);

        match trigger {
            // Inventory arrival: re-quote ONLY buckets whose ask inventory
            // changed since last placement (a fresh bucket has no `last`, so a
            // non-zero inventory counts as a change). Everything else keeps its
            // daily price.
            Trigger::InventoryArrival => {
                if last.get(&pool_key).map(|q| q.ask_target).unwrap_or(0) == ask_target {
                    continue;
                }
            }
            // Daily refresh: skip pools whose quotes are fresh, the fair hasn't
            // moved, and the target size is unchanged. At the daily cadence the
            // previous quote is a day old so this never fires (full reprice),
            // but it keeps a sub-daily scheduled cadence cheap.
            Trigger::Scheduled => {
                if let Some(prev) = last.get(&pool_key) {
                    let age_ms = now.saturating_sub(prev.at_ms);
                    let drift_bps = (mid_raw.abs_diff(prev.mid_raw) as u128)
                        .saturating_mul(10_000)
                        .checked_div(prev.mid_raw.max(1) as u128)
                        .unwrap_or(u128::MAX);
                    if age_ms < p.cfg.max_quote_age_secs.saturating_mul(1_000)
                        && drift_bps < p.cfg.requote_drift_bps as u128
                        && ask_target == prev.ask_target
                    {
                        continue;
                    }
                }
            }
        }

        // Sweep this pool's wallet call coins into the BM so the ask can rest
        // them (they arrive when the bot buys options as the Trader MM).
        if wallet_base > 0 {
            *deposits.entry(b.call_coin_type.clone()).or_default() += wallet_base;
        }
        // We deposit the whole inventory but rest a hair less, leaving
        // `fee_headroom_bps` of it in the BM to cover the input-coin maker fee
        // DeepBook withdraws on top of the order size — otherwise
        // place_limit_order aborts EBalanceManagerBalanceTooLow (we can't
        // deposit more base than we hold, so the ask must be the side that
        // shrinks). A no-op for a fixed `quote_size` ask, which is already far
        // below inventory.
        let ask_ceiling = reserve_fee_headroom(ask_inventory, p.cfg.fee_headroom_bps);
        let ask_qty = (ask_target.min(ask_ceiling) / lot) * lot;

        // Bid demand (mirrors the ask in full-inventory mode); summed so we can
        // source the settlement to fund it before placing.
        desired_bid_notional = desired_bid_notional
            .saturating_add(bid_target as u128 * bid_raw as u128 / 1_000_000_000);

        plans.push(PoolPlan {
            pool_id,
            pool_key,
            base_coin_type: b.call_coin_type.clone(),
            quote_coin_type: b.settlement_coin_type.clone(),
            ask_raw,
            bid_raw,
            ask_qty,
            bid_target,
            min_size,
            lot,
            mid_raw,
            ask_target,
        });
    }

    // Pull settlement (TUSDC) from the liquidity source so the bids can mirror
    // the asks. Target the desired bid notional plus a buffer covering bid
    // rounding and the placement's settlement-side fee headroom. Best effort:
    // a source that can't supply just leaves the budget where it is.
    if p.cfg.settlement_topup_enabled && desired_bid_notional > 0 {
        let buffer = desired_bid_notional
            .saturating_mul(p.cfg.fee_headroom_bps as u128)
            / 10_000;
        let target = desired_bid_notional.saturating_add(buffer).min(u64::MAX as u128) as u64;
        p.liquidity
            .ensure_wallet_balance(&wrap.client, &wrap.signer, &p.settlement_coin_type, target)
            .await;
    }

    // Settlement we can commit across every bid this cycle: BM balance + wallet
    // (now topped up). Read after the top-up so the budget reflects it.
    let bm_quote = bm_balance(
        &wrap.client,
        wrap.signer.address,
        &p.handles,
        bm_id,
        &p.settlement_coin_type,
    )
    .await
    .unwrap_or(0);
    let wallet_quote: u64 = wrap
        .client
        .coin_read_api()
        .get_balance(wrap.signer.address, Some(p.settlement_coin_type.clone()))
        .await
        .map(|bal| bal.total_balance.min(u64::MAX as u128) as u64)
        .unwrap_or(0);
    let mut quote_budget: u128 = bm_quote as u128 + wallet_quote as u128;
    let mut quote_used: u128 = 0;

    // Placement pass: size each bid against the shared budget, then emit.
    for plan in plans {
        let want_notional = plan.bid_target as u128 * plan.bid_raw as u128 / 1_000_000_000;
        let give = want_notional.min(quote_budget);
        let affordable = (give * 1_000_000_000 / plan.bid_raw as u128).min(u64::MAX as u128) as u64;
        let bid_qty = (plan.bid_target.min(affordable) / plan.lot) * plan.lot;
        let used = bid_qty as u128 * plan.bid_raw as u128 / 1_000_000_000;
        quote_budget = quote_budget.saturating_sub(used);
        quote_used = quote_used.saturating_add(used);

        let quote_plan = QuotePlan {
            ask: (plan.ask_qty >= plan.min_size)
                .then_some(QuoteSide { price_raw: plan.ask_raw, quantity: plan.ask_qty }),
            bid: (bid_qty >= plan.min_size)
                .then_some(QuoteSide { price_raw: plan.bid_raw, quantity: bid_qty }),
            expire_timestamp_ms: now_expire,
        };

        if quote_plan.bid.is_none() && quote_plan.ask.is_none() {
            if quoted_pools.contains_key(&plan.pool_key) {
                refreshes.push(PoolRefresh {
                    pool_id: plan.pool_id,
                    base_coin_type: plan.base_coin_type.clone(),
                    quote_coin_type: plan.quote_coin_type.clone(),
                    plan: quote_plan,
                });
                book.push((plan.pool_key, None));
            }
            continue;
        }

        refreshes.push(PoolRefresh {
            pool_id: plan.pool_id,
            base_coin_type: plan.base_coin_type.clone(),
            quote_coin_type: plan.quote_coin_type.clone(),
            plan: quote_plan,
        });
        book.push((
            plan.pool_key,
            Some((plan.mid_raw, plan.ask_target, plan.base_coin_type, plan.quote_coin_type)),
        ));
    }

    // Settlement deposit = total bid notional (grossed up by the maker-fee
    // headroom DeepBook withdraws on top of it) beyond what the BM already
    // holds, capped by the wallet. The top-up above sourced this buffer into
    // the wallet; depositing it — rather than leaving it stranded there — is
    // what gives the resting bid the slack its `withdraw_with_proof` needs.
    let quote_target = quote_used
        .saturating_add(quote_used.saturating_mul(p.cfg.fee_headroom_bps as u128) / 10_000);
    let quote_deposit =
        quote_target.saturating_sub(bm_quote as u128).min(wallet_quote as u128) as u64;
    if quote_deposit > 0 {
        deposits.insert(p.settlement_coin_type.clone(), quote_deposit);
    }

    if refreshes.is_empty() {
        return Ok(());
    }
    let deposits_vec: Vec<(String, u64)> = deposits.into_iter().collect();
    match refresh_pools_batched(
        &wrap.client,
        &wrap.signer,
        &p.handles,
        bm_id,
        &deposits_vec,
        &refreshes,
        p.cfg.max_pools_per_tx,
        p.cfg.gas_budget,
    )
    .await
    {
        Ok(resps) => {
            for (pool_key, upd) in book {
                match upd {
                    Some((mid_raw, ask_target, call, settle)) => {
                        last.insert(
                            pool_key.clone(),
                            LastQuote { mid_raw, ask_target, at_ms: now },
                        );
                        quoted_pools.insert(pool_key, (call, settle));
                    }
                    None => {
                        last.remove(&pool_key);
                        quoted_pools.remove(&pool_key);
                    }
                }
            }
            tracing::info!(
                pools = refreshes.len(),
                txs = resps.len(),
                deposits = deposits_vec.len(),
                "deepbook quotes refreshed (batched)"
            );
        }
        Err(e) => {
            tracing::error!(
                alert_id = "tx-failed-mm-bot-deepbook",
                error = %format!("{e:#}"),
                pools = refreshes.len(),
                "batched deepbook refresh tx failed; leaving books as-is"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_targets_default_mode_is_fixed_quote_size() {
        let cfg = DeepBookQuoterConfig::default();
        // Inventory is ignored: both sides target quote_size.
        assert_eq!(size_targets(&cfg, 0), (cfg.quote_size, cfg.quote_size));
        assert_eq!(size_targets(&cfg, 99_000_000), (cfg.quote_size, cfg.quote_size));
    }

    #[test]
    fn size_targets_full_inventory_lists_everything_and_mirrors_bid() {
        let cfg = DeepBookQuoterConfig {
            quote_full_inventory: true,
            ..DeepBookQuoterConfig::default()
        };
        // Ask = whole inventory; bid mirrors it.
        assert_eq!(size_targets(&cfg, 50_000_000), (50_000_000, 50_000_000));
        // Empty inventory: no ask, but the bid floors at quote_size so the
        // book stays two-sided and the bot can still acquire inventory.
        assert_eq!(size_targets(&cfg, 0), (0, cfg.quote_size));
        // Inventory below quote_size: ask lists it all, bid keeps the floor.
        assert_eq!(size_targets(&cfg, 1_000), (1_000, cfg.quote_size));
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

    #[test]
    fn reserve_fee_headroom_leaves_slack_for_the_maker_fee() {
        // The whole point: rest strictly less than what's deposited, so the BM
        // still holds the order size + DeepBook's input-coin maker fee.
        // Default 100 bps reserve dwarfs the ~6.25 bps (5 bps maker × 1.25
        // non-DEEP penalty) DeepBook actually withdraws.
        let inventory = 1_000_000_000u64;
        let reserved = reserve_fee_headroom(inventory, 100);
        assert_eq!(reserved, 990_000_000); // 1% kept back
        let maker_fee_bps = 6.25_f64;
        let fee_on_order = (reserved as f64 * maker_fee_bps / 10_000.0).ceil() as u64;
        // order + fee must fit inside the deposited inventory.
        assert!(reserved + fee_on_order <= inventory);
        // Resting the full inventory (the old behavior) would NOT fit.
        assert!(inventory + (inventory as f64 * maker_fee_bps / 10_000.0) as u64 > inventory);
    }

    #[test]
    fn reserve_fee_headroom_is_noop_at_zero_and_saturates() {
        assert_eq!(reserve_fee_headroom(1_000_000, 0), 1_000_000);
        // A pathological >100% reserve can't underflow into a huge size.
        assert_eq!(reserve_fee_headroom(1_000_000, 20_000), 0);
    }
}
