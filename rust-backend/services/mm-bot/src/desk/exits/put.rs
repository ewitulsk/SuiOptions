//! Put exercise (SO-443, doc 08 §4.4 / 00-plan §5): the policy, the
//! three-path waterfall and the redundant near-expiry sweep.
//!
//! A held put is exercised only when
//!
//! ```text
//! strike payout − underlying acquisition cost − swap fees/slippage
//!   − flash cost − gas ≥ min_profit
//! min_profit = max($10 settlement, 5 bps × payout, 2 × route uncertainty)
//! ```
//!
//! and then through the FIRST profitable AVAILABLE atomic route, in
//! order: own/vault underlying → base flash loan → quote flash loan
//! (`sui_tx::tx::put_exercise`). Every route is pre-simulated with
//! explicit max-input / min-output, exact repayment, the configured
//! pool allowlist, a flash-capacity check, a gas bound and an on-chain
//! minimum-profit assertion; any failure aborts the whole PTB. Large
//! positions ladder in slices sized so the ladder cannot cross expiry.
//!
//! The Sui PTB is atomic; the LONG-perp hedge unwind is not. After a
//! successful slice the hedge close (a SELL of `|Δ| × units`) is sent
//! through the market's primary [`HedgeVenue`] immediately and its
//! delay is logged as its own line.
//!
//! Custody legs mirror the call path: wallet float coins use the wallet
//! routes (own underlying / base flash / quote flash); VaultMm coin-
//! custody positions use `vault_mm::exercise_put_coin` + the
//! deepbook-adapter repurchase on an allowlisted pool (vault-underlying
//! only — flash routes are wallet-only, exactly like calls).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use serde::Deserialize;
use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::put_exercise::{
    self, lot_round_up, quote_needed_for_base, PoolLiquidity, PutPath, PutPtbSpec,
    PutSubmitRefs, VaultPutPtbArgs, VaultPutPtbSpec,
};
use sui_tx::tx::{clock_arg, shared_object_arg, submit_ptb};
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;

use crate::desk::book::{self, CoinPosition, Holding};
use crate::desk::hedge::{HedgeCommand, HedgeEvent, HedgeOrder};
use crate::desk::model::MarketModel;
use crate::desk::CuratorRefs;

use super::{ExitsConfig, ExitsParams, ALERT_ID};

/// `[desk.exits.put]` knobs. Defaults per doc 08 §0.4.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PutExerciseConfig {
    pub enabled: bool,
    /// Minimum-profit term 1: settlement-equivalent dollars.
    pub min_profit_usd: f64,
    /// Minimum-profit term 2: bps of the strike payout.
    pub min_profit_bps: f64,
    /// Route uncertainty as a bps bound on the strike payout (Pyth
    /// confidence/staleness, router quote age, tick rounding,
    /// dev-inspect-to-inclusion drift). Also the repurchase slippage
    /// allowance folded into the PTB's explicit max-input.
    pub route_uncertainty_bps: f64,
    /// Minimum-profit term 3: this × route uncertainty.
    pub route_uncertainty_mult: f64,
    /// Taker fee bound on the repurchase, bps of acquisition cost
    /// (whitelisted DeepBook pools charge none).
    pub swap_fee_bps: f64,
    /// Flash-loan fee bound, bps of principal (DeepBook v3: none).
    pub flash_fee_bps: f64,
    /// Gas allowance per slice, settlement-equivalent dollars.
    pub gas_cost_usd: f64,
    /// Dev-inspect gas (MIST) above which a slice is not submitted.
    pub max_gas_mist: u64,
    /// Per-slice inclusion allowance when sizing a ladder against expiry.
    pub ladder_tx_secs: u64,
    /// No slice starts inside this margin before expiry.
    pub expiry_margin_secs: u64,
    /// Ask-ladder depth read for route pricing.
    pub ask_ticks: u64,
}

impl Default for PutExerciseConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_profit_usd: 10.0,
            min_profit_bps: 5.0,
            route_uncertainty_bps: 20.0,
            route_uncertainty_mult: 2.0,
            swap_fee_bps: 0.0,
            flash_fee_bps: 0.0,
            gas_cost_usd: 0.05,
            max_gas_mist: 100_000_000,
            ladder_tx_secs: 30,
            expiry_margin_secs: 120,
            ask_ticks: 50,
        }
    }
}

// ── pure policy ────────────────────────────────────────────────────────

/// `put_bucket::exercise_payout` mirror: floor(amount × strike).
pub fn strike_payout(amount: u64, strike: u128, strike_scale: u8) -> u64 {
    let divisor = 10u128.pow(strike_scale as u32);
    u64::try_from(amount as u128 * strike / divisor).unwrap_or(u64::MAX)
}

fn bps_of(amount: u64, bps: f64) -> u64 {
    (amount as f64 * bps / 10_000.0).ceil().max(0.0) as u64
}

fn usd_raw(usd: f64, settlement_decimals: u8) -> u64 {
    (usd * 10f64.powi(settlement_decimals as i32)).ceil().max(0.0) as u64
}

/// `max($ term, bps × payout, mult × route uncertainty)` in settlement
/// raw units.
pub fn min_profit(cfg: &PutExerciseConfig, payout: u64, settlement_decimals: u8) -> u64 {
    usd_raw(cfg.min_profit_usd, settlement_decimals)
        .max(bps_of(payout, cfg.min_profit_bps))
        .max(bps_of(route_uncertainty(cfg, payout), cfg.route_uncertainty_mult * 10_000.0))
}

/// The conservative cash bound on route drift for one slice.
pub fn route_uncertainty(cfg: &PutExerciseConfig, payout: u64) -> u64 {
    bps_of(payout, cfg.route_uncertainty_bps)
}

/// What the desk can draw on for one slice.
#[derive(Clone, Debug, Default)]
pub struct PutLiquidity {
    /// Underlying already held by the leg (wallet float, or vault free
    /// balance for the custody leg) — the vault-underlying route.
    pub own_underlying: u64,
    /// The allowlisted UNDERLYING/SETTLEMENT spot pool.
    pub pool: PoolLiquidity,
}

/// One slice's chosen route and bounds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PutPlan {
    pub path: PutPath,
    pub amount: u64,
    pub payout: u64,
    /// Explicit max settlement spent on the repurchase (acquisition +
    /// fee + slippage allowance); the quote-flash principal.
    pub max_quote_in: u64,
    pub min_profit: u64,
    /// Modeled net after every cost, before the minimum.
    pub expected_net: u64,
}

/// Why no route was taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanReject {
    /// The ask ladder can't supply the replacement underlying.
    NoRoute,
    /// Modeled net is below the minimum on every route.
    Profit { net: i128, min_profit: u64 },
    /// Every profitable route lacks capacity (own underlying / flash).
    Capacity,
}

impl std::fmt::Display for PlanReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanReject::NoRoute => write!(f, "no repurchase route (ask ladder too shallow)"),
            PlanReject::Profit { net, min_profit } => {
                write!(f, "modeled net {net} below minimum profit {min_profit}")
            }
            PlanReject::Capacity => write!(f, "no route has capacity"),
        }
    }
}

/// Choose the first profitable available route for `amount` put units.
pub fn plan_slice(
    cfg: &PutExerciseConfig,
    amount: u64,
    strike: u128,
    strike_scale: u8,
    settlement_decimals: u8,
    liq: &PutLiquidity,
) -> Result<PutPlan, PlanReject> {
    let payout = strike_payout(amount, strike, strike_scale);
    let base_needed = lot_round_up(amount, liq.pool.lot_size, liq.pool.min_size)
        .ok_or(PlanReject::NoRoute)?;
    let acquisition = quote_needed_for_base(&liq.pool.asks, base_needed)
        .ok_or(PlanReject::NoRoute)?;
    let swap_fee = bps_of(acquisition, cfg.swap_fee_bps);
    let uncertainty = route_uncertainty(cfg, payout);
    let max_quote_in = acquisition.saturating_add(swap_fee).saturating_add(uncertainty);
    let min_profit = min_profit(cfg, payout, settlement_decimals);
    let gas = usd_raw(cfg.gas_cost_usd, settlement_decimals);

    let mut best_reject = PlanReject::Capacity;
    for path in PutPath::ORDER {
        let flash_cost = match path {
            PutPath::VaultUnderlying => 0,
            PutPath::BaseFlash => bps_of(acquisition, cfg.flash_fee_bps),
            PutPath::QuoteFlash => bps_of(max_quote_in, cfg.flash_fee_bps),
        };
        let net = payout as i128
            - acquisition as i128
            - swap_fee as i128
            - flash_cost as i128
            - gas as i128;
        // The on-chain assertion needs the whole max-input plus the
        // minimum to fit inside the payout.
        let onchain_ok = max_quote_in.saturating_add(min_profit) <= payout;
        if net < min_profit as i128 || !onchain_ok {
            best_reject = PlanReject::Profit { net, min_profit };
            continue;
        }
        let available = match path {
            PutPath::VaultUnderlying => liq.own_underlying >= amount,
            PutPath::BaseFlash => liq.pool.base_balance >= amount,
            PutPath::QuoteFlash => liq.pool.quote_balance >= max_quote_in,
        };
        if !available {
            continue;
        }
        return Ok(PutPlan {
            path,
            amount,
            payout,
            max_quote_in,
            min_profit,
            expected_net: (net - min_profit as i128).max(0) as u64 + min_profit,
        });
    }
    Err(best_reject)
}

/// Slice `amount` into a ladder that cannot cross expiry: at most
/// `max_slice` per tx, and no more slices than fit in
/// `remaining_ms − margin` at `tx_ms` each (bigger slices when the
/// clock, not the chunk, binds). Empty when expiry is inside the margin.
pub fn ladder(
    amount: u64,
    max_slice: u64,
    remaining_ms: u64,
    tx_ms: u64,
    margin_ms: u64,
) -> Vec<u64> {
    if amount == 0 || remaining_ms <= margin_ms {
        return Vec::new();
    }
    let max_slice = max_slice.max(1);
    let allowed = ((remaining_ms - margin_ms) / tx_ms.max(1)).max(1);
    let wanted = amount.div_ceil(max_slice);
    let n = wanted.min(allowed);
    let slice = amount.div_ceil(n);
    let mut out = Vec::with_capacity(n as usize);
    let mut left = amount;
    while left > 0 {
        let s = left.min(slice);
        out.push(s);
        left -= s;
    }
    out
}

/// The exercise-timing rule for a held put: ITM, and either inside the
/// near-expiry sweep window (every economically exercisable put is
/// attempted before expiry) or American-optimal — carry on the strike
/// cash beats the remaining time value with margin. Pure; the caller
/// prices `carry` and `time_value` per unit.
pub fn put_exercise_wanted(
    cfg: &ExitsConfig,
    spot: f64,
    strike: f64,
    hours_to_expiry: f64,
    carry: f64,
    time_value: f64,
) -> bool {
    let itm = spot < strike;
    let near_expiry = hours_to_expiry <= cfg.near_expiry_hours;
    itm && (near_expiry || carry >= time_value * cfg.carry_mult)
}

/// Per-unit carry and time value of a held put under `model`.
pub fn put_carry_and_time_value(
    model: &MarketModel,
    spot: f64,
    strike: f64,
    t_years: f64,
) -> (f64, f64) {
    let (sigma, _) = model.sigma(spot, strike, t_years);
    let fair = model.fair_per_unit(true, spot, strike, t_years, sigma);
    let intrinsic = (strike - spot).max(0.0);
    let carry = strike * model.rate * t_years;
    (carry, (fair - intrinsic).max(0.0))
}

// ── execution ──────────────────────────────────────────────────────────

/// Hedge-close order ids live in their own half of the id space so they
/// never collide with the rebalancer's process-local counter.
static NEXT_CLOSE_ID: AtomicU64 = AtomicU64::new(1 << 63);

/// The deepbook-adapter package + shared `PoolAllowlist` the vault-custody
/// repurchase needs.
#[derive(Clone, Copy, Debug)]
pub struct AdapterRefs {
    pub package: ObjectID,
    pub pool_allowlist: ObjectID,
}

struct Market<'a> {
    model: &'a MarketModel,
    spot: f64,
    t_years: f64,
    decimals: u8,
}

/// Run the put waterfall for one holding this tick: wallet leg (float
/// coins) and vault leg (coin-custody positions), each laddered.
/// Returns the units exercised.
pub async fn run(
    p: &ExitsParams,
    wrap: &SuiClientWrapper,
    h: &Holding,
    mi: usize,
    spot: f64,
    now_ms: u64,
) -> Result<u64> {
    let cfg = &p.cfg.put;
    let model = &p.models[mi];
    let Some(pool) = p.cfg.spot_pools.get(&model.symbol) else {
        tracing::warn!(
            symbol = %model.symbol,
            "put exercise wanted but no [desk.exits.spot_pools] entry (pool allowlist); holding"
        );
        return Ok(0);
    };
    let pool = ObjectID::from_hex_literal(pool)?;
    let handles = p.handles.as_ref().context("no deepbook handles")?;
    let deep = p.deep_coin_type.as_deref().context("no deep coin type")?;
    let m = Market {
        model,
        spot,
        t_years: h.expiry_ms.saturating_sub(now_ms) as f64 / 1000.0 / 86_400.0 / 365.0,
        decimals: p.market_feeds[mi].1,
    };
    let remaining_ms = h.expiry_ms.saturating_sub(now_ms);
    let liq_pool = put_exercise::pool_liquidity(
        &wrap.client,
        wrap.signer.address,
        handles.package,
        pool,
        &h.asset_coin_type,
        &h.settlement_coin_type,
        cfg.ask_ticks,
    )
    .await
    .context("reading spot pool liquidity")?;
    let mut exercised = 0u64;

    // Wallet leg (float coins).
    if h.amount_wallet > 0 {
        let own = wallet_balance(wrap, &h.asset_coin_type).await;
        let liq = PutLiquidity { own_underlying: own, pool: liq_pool.clone() };
        let slices = ladder(
            h.amount_wallet,
            p.cfg.max_slice,
            remaining_ms,
            cfg.ladder_tx_secs * 1000,
            cfg.expiry_margin_secs * 1000,
        );
        for slice in slices {
            let plan = match plan_slice(
                cfg,
                slice,
                h.strike,
                h.strike_scale,
                p.settlement_decimals,
                &liq,
            ) {
                Ok(plan) => plan,
                Err(reject) => {
                    tracing::info!(
                        bucket = %h.bucket_id.to_hex(),
                        slice,
                        %reject,
                        "put slice not exercisable (wallet leg)"
                    );
                    break;
                }
            };
            let spec = PutPtbSpec {
                deepbook_package: handles.package,
                core_package: p.core_package,
                underlying_type: &h.asset_coin_type,
                settlement_type: &h.settlement_coin_type,
                put_coin_type: &h.option_coin_type,
                deep_coin_type: deep,
                amount: plan.amount,
                payout: plan.payout,
                max_quote_in: plan.max_quote_in,
                min_profit: plan.min_profit,
                recipient: p.vault_address,
            };
            let refs = PutSubmitRefs {
                spot_pool: pool,
                bucket: ObjectID::new(*h.bucket_id.as_bytes()),
                gas_budget: p.cfg.gas_budget,
                max_gas_mist: cfg.max_gas_mist,
            };
            let started = Instant::now();
            let res = put_exercise::submit_put_exercise(
                &wrap.client,
                &wrap.signer,
                &spec,
                &refs,
                plan.path,
            )
            .await;
            match res {
                Ok(resp) => {
                    tracing::info!(
                        bucket = %h.bucket_id.to_hex(),
                        path = plan.path.label(),
                        amount = plan.amount,
                        payout = plan.payout,
                        max_quote_in = plan.max_quote_in,
                        min_profit = plan.min_profit,
                        digest = %sui_tx::tx::tx_digest(&resp),
                        "put exercised (residual settlement → vault)"
                    );
                    metrics::counter!("mm_desk_put_exercise_total", "path" => plan.path.label())
                        .increment(1);
                    exercised += plan.amount;
                    hedge_close(p, h, mi, &m, plan.amount, started).await;
                }
                Err(e) => {
                    // The PTB aborts atomically: nothing signed on a
                    // pre-simulation failure, everything reverted on-chain.
                    tracing::error!(
                        alert_id = ALERT_ID,
                        bucket = %h.bucket_id.to_hex(),
                        path = plan.path.label(),
                        amount = plan.amount,
                        error = %format!("{e:#}"),
                        "put exercise tx failed (wallet leg)"
                    );
                    break;
                }
            }
        }
    }

    // Vault leg (coin-custody positions), minus resting exchange asks.
    let listed = p.book.read().listed_units(&h.bucket_id);
    let vault_units = h.amount_coin_positions().saturating_sub(listed);
    if vault_units == 0 {
        return Ok(exercised);
    }
    let hold = |why: &str| {
        tracing::info!(
            bucket = %h.bucket_id.to_hex(),
            vault_held = vault_units,
            "vault-custody put exit wanted but {why}; holding"
        );
    };
    let Some(refs) = p.curator.as_ref() else {
        hold("curator refs unresolved");
        return Ok(exercised);
    };
    let Some(adapter) = p.deepbook_adapter.as_ref() else {
        hold("deepbook adapter refs missing");
        return Ok(exercised);
    };
    // SO-418 risk gate: the session takes vault FREE underlying.
    if p.shared.risk_off.load(Ordering::Relaxed) {
        hold("the vault is risk-off");
        return Ok(exercised);
    }
    let free_underlying = book::free_balance_of(
        wrap,
        refs.trading_vault_package,
        refs.vault_id,
        &h.asset_coin_type,
    )
    .await
    .unwrap_or(0);
    let liq = PutLiquidity { own_underlying: free_underlying, pool: liq_pool };
    let mut budget = vault_units;
    for cp in &h.coin_positions {
        if budget == 0 {
            break;
        }
        let units = cp.amount.min(budget);
        let slices = ladder(
            units,
            p.cfg.max_slice,
            remaining_ms,
            cfg.ladder_tx_secs * 1000,
            cfg.expiry_margin_secs * 1000,
        );
        for slice in slices {
            // Only the vault-underlying route exists for custody coins.
            let plan = match plan_slice(
                cfg,
                slice,
                h.strike,
                h.strike_scale,
                p.settlement_decimals,
                &liq,
            ) {
                Ok(plan) if plan.path == PutPath::VaultUnderlying => plan,
                Ok(plan) => {
                    tracing::info!(
                        bucket = %h.bucket_id.to_hex(),
                        slice,
                        path = plan.path.label(),
                        "vault free underlying short for the slice; holding custody coins"
                    );
                    break;
                }
                Err(reject) => {
                    tracing::info!(
                        bucket = %h.bucket_id.to_hex(),
                        slice,
                        %reject,
                        "put slice not exercisable (vault leg)"
                    );
                    break;
                }
            };
            let started = Instant::now();
            match vault_exercise(p, wrap, refs, adapter, h, cp, &plan, pool).await {
                Ok(()) => {
                    metrics::counter!("mm_desk_put_exercise_total", "path" => "vault_custody")
                        .increment(1);
                    exercised += plan.amount;
                    budget -= plan.amount;
                    // The repurchase restores the delivered underlying, so
                    // `liq.own_underlying` is unchanged for the next slice.
                    hedge_close(p, h, mi, &m, plan.amount, started).await;
                }
                Err(e) => {
                    tracing::error!(
                        alert_id = ALERT_ID,
                        bucket = %h.bucket_id.to_hex(),
                        coin_position = %cp.position_id.to_hex(),
                        amount = plan.amount,
                        error = %format!("{e:#}"),
                        "put exercise tx failed (vault leg)"
                    );
                    return Ok(exercised);
                }
            }
        }
    }
    Ok(exercised)
}

async fn wallet_balance(wrap: &SuiClientWrapper, coin_type: &str) -> u64 {
    match sui_types::parse_sui_struct_tag(coin_type) {
        Ok(tag) => wrap
            .client
            .balance(wrap.signer.address, &tag)
            .await
            .map(|b| u64::try_from(b).unwrap_or(u64::MAX))
            .unwrap_or(0),
        Err(_) => 0,
    }
}

/// `vault_mm::exercise_put_coin` + `deepbook_adapter::taker_swap_quote_for_base`
/// in one curator-session PTB, dev-inspected (status + gas bound) first.
#[allow(clippy::too_many_arguments)]
async fn vault_exercise(
    p: &ExitsParams,
    wrap: &SuiClientWrapper,
    refs: &CuratorRefs,
    adapter: &AdapterRefs,
    h: &Holding,
    cp: &CoinPosition,
    plan: &PutPlan,
    pool: ObjectID,
) -> Result<()> {
    let cfg = &p.cfg.put;
    let build = |pt: &mut ProgrammableTransactionBuilder, args: VaultPutPtbArgs| {
        let spec = VaultPutPtbSpec {
            trading_vault_package: refs.trading_vault_package,
            deepbook_adapter_package: adapter.package,
            underlying_type: &h.asset_coin_type,
            settlement_type: &h.settlement_coin_type,
            put_coin_type: &h.option_coin_type,
            coin_position_id: ObjectID::new(*cp.position_id.as_bytes()),
            amount: plan.amount,
            payout: plan.payout,
            max_quote_in: plan.max_quote_in,
            min_profit: plan.min_profit,
        };
        put_exercise::build_vault_put_exercise(pt, &spec, &args)
    };
    let mut pt = ProgrammableTransactionBuilder::new();
    let args = vault_args(wrap, refs, adapter, h, pool, &mut pt).await?;
    build(&mut pt, args)?;
    let programmable = pt.finish();
    let sim_refs = PutSubmitRefs {
        spot_pool: pool,
        bucket: ObjectID::new(*h.bucket_id.as_bytes()),
        gas_budget: p.cfg.gas_budget,
        max_gas_mist: cfg.max_gas_mist,
    };
    put_exercise::presimulate(&wrap.client, wrap.signer.address, &programmable, &sim_refs).await?;
    // Rebuild for submission: the CuratorCap ref must be current.
    let mut pt = ProgrammableTransactionBuilder::new();
    let args = vault_args(wrap, refs, adapter, h, pool, &mut pt).await?;
    build(&mut pt, args)?;
    let resp = submit_ptb(
        &wrap.client,
        &wrap.signer,
        pt,
        p.cfg.gas_budget,
        "desk vault put exercise",
    )
    .await?;
    tracing::info!(
        bucket = %h.bucket_id.to_hex(),
        coin_position = %cp.position_id.to_hex(),
        amount = plan.amount,
        payout = plan.payout,
        max_quote_in = plan.max_quote_in,
        min_profit = plan.min_profit,
        digest = %sui_tx::tx::tx_digest(&resp),
        "exercised vault-custody puts with vault underlying (repurchased; residual → vault)"
    );
    Ok(())
}

async fn vault_args(
    wrap: &SuiClientWrapper,
    refs: &CuratorRefs,
    adapter: &AdapterRefs,
    h: &Holding,
    pool: ObjectID,
    pt: &mut ProgrammableTransactionBuilder,
) -> Result<VaultPutPtbArgs> {
    let vault = pt.obj(shared_object_arg(&wrap.client, refs.vault_id, true).await?)?;
    let cap = pt.obj(sui_tx::tx::owned_object_arg(&wrap.client, refs.curator_cap).await?)?;
    let reg = pt.obj(shared_object_arg(&wrap.client, refs.integration_registry, false).await?)?;
    let allowlist = pt.obj(shared_object_arg(&wrap.client, adapter.pool_allowlist, false).await?)?;
    let pool = pt.obj(shared_object_arg(&wrap.client, pool, true).await?)?;
    let bucket = pt.obj(
        shared_object_arg(&wrap.client, ObjectID::new(*h.bucket_id.as_bytes()), true).await?,
    )?;
    let clock = clock_arg(pt)?;
    Ok(VaultPutPtbArgs { vault, cap, reg, allowlist, pool, bucket, clock })
}

/// Close the exercised units' share of the LONG perp hedge: a SELL of
/// `|Δ| × units` on the market's primary venue, scheduled immediately
/// after the PTB lands; the delay from PTB success is its own log line
/// and histogram (doc 08 §4.4: the unwind is not atomic with the chain).
async fn hedge_close(
    p: &ExitsParams,
    h: &Holding,
    mi: usize,
    m: &Market<'_>,
    units: u64,
    ptb_done: Instant,
) {
    let Some(venue) = p.hedge_venues.get(mi) else {
        return;
    };
    let strike = h.strike_scaled();
    let (sigma, _) = m.model.sigma(m.spot, strike, m.t_years);
    let delta = m.model.greeks_per_unit(true, m.spot, strike, m.t_years, sigma).delta;
    // Held put: book delta = Δ × units (Δ < 0), hedge = −that (long);
    // closing sells it back, i.e. size = Δ × units.
    let size = delta * units as f64;
    if size == 0.0 {
        return;
    }
    let id = NEXT_CLOSE_ID.fetch_add(1, Ordering::Relaxed);
    let order = HedgeOrder { id, size_units: size, spot: m.spot };
    match venue.execute(HedgeCommand::Submit(order)).await {
        Ok(events) => {
            let delay_ms = ptb_done.elapsed().as_millis() as u64;
            let filled: f64 = events
                .iter()
                .map(|e| match e {
                    HedgeEvent::Filled(f) | HedgeEvent::PartiallyFilled(f) => f.size_units,
                    _ => 0.0,
                })
                .sum();
            metrics::histogram!("mm_desk_put_exercise_hedge_close_delay_ms")
                .record(delay_ms as f64);
            tracing::info!(
                bucket = %h.bucket_id.to_hex(),
                venue = venue.name(),
                order = id,
                size,
                filled,
                delay_ms,
                decimals = m.decimals,
                "put exercise hedge close scheduled"
            );
            if let Some(HedgeEvent::Rejected { reason, .. }) =
                events.iter().find(|e| matches!(e, HedgeEvent::Rejected { .. }))
            {
                tracing::error!(
                    alert_id = ALERT_ID,
                    venue = venue.name(),
                    order = id,
                    reason,
                    "put exercise hedge close rejected"
                );
            }
        }
        Err(e) => tracing::error!(
            alert_id = ALERT_ID,
            venue = venue.name(),
            order = id,
            error = %format!("{e:#}"),
            "put exercise hedge close failed"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> PutExerciseConfig {
        PutExerciseConfig::default()
    }

    /// 9-dec underlying, 6-dec settlement, strike $4.00/unit = 0.004
    /// settlement-raw per underlying-raw → 4_000 at scale 6 (the
    /// `strike_cost` convention): 5 units pay out $20.
    const STRIKE: u128 = 4_000;
    const SCALE: u8 = 6;
    const DEC: u8 = 6;
    const AMOUNT: u64 = 5_000_000_000;

    /// Ask ladder at $3.50/unit: 0.0035 quote-atomic per base-atomic →
    /// price_raw 3_500_000 (× 1e9).
    fn pool(base_balance: u64, quote_balance: u64) -> PoolLiquidity {
        PoolLiquidity {
            base_balance,
            quote_balance,
            lot_size: 1_000,
            min_size: 10_000,
            asks: vec![(3_500_000, 1_000_000_000_000)],
        }
    }

    #[test]
    fn payout_mirrors_apply_strike_floor() {
        assert_eq!(strike_payout(AMOUNT, STRIKE, SCALE), 20_000_000);
        assert_eq!(strike_payout(1, 15, 1), 1); // 1.5 floors to 1
    }

    #[test]
    fn min_profit_is_the_max_of_the_three_terms() {
        let c = cfg();
        // $10 dominates a $20 payout (5 bps = $0.01, 2 × 20 bps = $0.08).
        assert_eq!(min_profit(&c, 20_000_000, DEC), 10_000_000);
        // A $1m payout: 5 bps = $500, 2 × 20 bps = $4000 → uncertainty term.
        assert_eq!(min_profit(&c, 1_000_000_000_000, DEC), 4_000_000_000);
        // Uncertainty off: bps term wins.
        let c2 = PutExerciseConfig { route_uncertainty_bps: 0.0, ..cfg() };
        assert_eq!(min_profit(&c2, 1_000_000_000_000, DEC), 500_000_000);
    }

    #[test]
    fn selects_vault_underlying_first_when_held() {
        let liq = PutLiquidity { own_underlying: AMOUNT, pool: pool(AMOUNT, 1_000_000_000) };
        // Payout $20 (5 × $4); acquisition 5 × $3.50 = $17.50; gas $0.05;
        // net $2.45 < $10 minimum → not profitable. Use 50 units instead.
        let big = AMOUNT * 50;
        let liq = PutLiquidity { own_underlying: big, ..liq };
        let plan = plan_slice(&cfg(), big, STRIKE, SCALE, DEC, &liq).unwrap();
        assert_eq!(plan.path, PutPath::VaultUnderlying);
        assert_eq!(plan.payout, 1_000_000_000); // $1000
        // acquisition $875 + 20 bps of $1000 ($2) slippage allowance.
        assert_eq!(plan.max_quote_in, 875_000_000 + 2_000_000);
        assert_eq!(plan.min_profit, 10_000_000);
        assert!(plan.max_quote_in + plan.min_profit <= plan.payout);
    }

    #[test]
    fn falls_through_to_base_flash_then_quote_flash() {
        let big = AMOUNT * 50;
        // No own underlying, base capacity present → base flash.
        let liq = PutLiquidity { own_underlying: 0, pool: pool(big, 0) };
        let plan = plan_slice(&cfg(), big, STRIKE, SCALE, DEC, &liq).unwrap();
        assert_eq!(plan.path, PutPath::BaseFlash);
        // Base liquidity unavailable → quote flash (needs max_quote_in of
        // quote capacity).
        let liq = PutLiquidity { own_underlying: 0, pool: pool(big - 1, 877_000_000) };
        let plan = plan_slice(&cfg(), big, STRIKE, SCALE, DEC, &liq).unwrap();
        assert_eq!(plan.path, PutPath::QuoteFlash);
        // Neither flash side has capacity → Capacity reject.
        let liq = PutLiquidity { own_underlying: 0, pool: pool(big - 1, 876_999_999) };
        assert_eq!(plan_slice(&cfg(), big, STRIKE, SCALE, DEC, &liq), Err(PlanReject::Capacity));
    }

    #[test]
    fn profit_bound_rejects_every_route() {
        // 5 units: net $2.45 < $10.
        let liq = PutLiquidity { own_underlying: AMOUNT, pool: pool(AMOUNT, 1_000_000_000) };
        match plan_slice(&cfg(), AMOUNT, STRIKE, SCALE, DEC, &liq) {
            Err(PlanReject::Profit { net, min_profit }) => {
                assert_eq!(net, 20_000_000 - 17_500_000 - 50_000);
                assert_eq!(min_profit, 10_000_000);
            }
            other => panic!("{other:?}"),
        }
        // OTM-ish: asks above strike → negative net.
        let mut deep = pool(AMOUNT * 50, 0);
        deep.asks = vec![(4_500_000, u64::MAX / 4)];
        let liq = PutLiquidity { own_underlying: AMOUNT * 50, pool: deep };
        assert!(matches!(
            plan_slice(&cfg(), AMOUNT * 50, STRIKE, SCALE, DEC, &liq),
            Err(PlanReject::Profit { net, .. }) if net < 0
        ));
    }

    #[test]
    fn shallow_ladder_or_sub_min_size_is_no_route() {
        let mut shallow = pool(AMOUNT * 50, 0);
        shallow.asks = vec![(3_500_000, 1_000)];
        let liq = PutLiquidity { own_underlying: AMOUNT * 50, pool: shallow };
        let r = plan_slice(&cfg(), AMOUNT * 50, STRIKE, SCALE, DEC, &liq);
        assert_eq!(r, Err(PlanReject::NoRoute));
        let liq = PutLiquidity { own_underlying: 1, pool: pool(1, 1) };
        assert_eq!(plan_slice(&cfg(), 1, STRIKE, SCALE, DEC, &liq), Err(PlanReject::NoRoute));
    }

    #[test]
    fn ladder_reconciles_and_never_crosses_expiry() {
        // 10 chunks of 1e9 fit in 10 min with 30 s per tx.
        let s = ladder(10_000_000_000, 1_000_000_000, 600_000, 30_000, 120_000);
        assert_eq!(s.len(), 10);
        assert_eq!(s.iter().sum::<u64>(), 10_000_000_000);
        // Only 4 tx fit (4 × 30 s inside 240 s − 120 s margin = 4) →
        // bigger slices, same total.
        let s = ladder(10_000_000_000, 1_000_000_000, 240_000, 30_000, 120_000);
        assert_eq!(s.len(), 4);
        assert_eq!(s.iter().sum::<u64>(), 10_000_000_000);
        assert!(s.iter().all(|&x| x <= 2_500_000_000));
        // Uneven remainder still reconciles.
        let s = ladder(7, 3, 1_000_000, 1, 0);
        assert_eq!(s, vec![3, 3, 1]);
        // Inside the expiry margin: nothing starts.
        assert!(ladder(7, 3, 100_000, 30_000, 120_000).is_empty());
        assert!(ladder(0, 3, 1_000_000, 30_000, 0).is_empty());
    }

    #[test]
    fn sweep_attempts_every_itm_put_near_expiry() {
        let c = ExitsConfig::default(); // near_expiry_hours 24, carry_mult 1.1
        // Fixture holdings: (spot, strike, hours, carry, time_value).
        let fixtures = [
            (3.0, 4.0, 1.0, 0.0, 0.5),   // ITM, near expiry, lots of TV → attempted
            (3.9, 4.0, 23.9, 0.0, 0.01), // ITM, just inside the window → attempted
            (3.0, 4.0, 48.0, 0.0, 0.5),  // ITM, far, TV > carry → hold
            (3.0, 4.0, 48.0, 0.6, 0.5),  // ITM, far, carry beats TV → attempted
            (5.0, 4.0, 1.0, 0.0, 0.0),   // OTM near expiry → never
        ];
        let got: Vec<bool> = fixtures
            .iter()
            .map(|&(s, k, h, carry, tv)| put_exercise_wanted(&c, s, k, h, carry, tv))
            .collect();
        assert_eq!(got, [true, true, false, true, false]);
        // Every economically exercisable (ITM) put inside the window is
        // attempted regardless of its remaining time value.
        for tv in [0.0, 0.1, 1.0, 10.0] {
            assert!(put_exercise_wanted(&c, 3.0, 4.0, 0.5, 0.0, tv));
        }
    }
}
