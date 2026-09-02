//! Put exercise EXECUTION (SO-443, doc 08 §4.4): the three-path atomic
//! waterfall and the redundant near-expiry sweep. The policy — timing
//! rule, minimum profit, route choice, ladder — is `desk_core::exits::put`,
//! re-exported here.
//!
//! Every route is pre-simulated with explicit max-input / min-output,
//! exact repayment, the configured pool allowlist, a flash-capacity
//! check, a gas bound and an on-chain minimum-profit assertion; any
//! failure aborts the whole PTB (`sui_tx::tx::put_exercise`).
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
use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::put_exercise::{
    self, PutPtbSpec, PutSubmitRefs, VaultPutPtbArgs, VaultPutPtbSpec,
};
use sui_tx::tx::{clock_arg, shared_object_arg, submit_ptb};
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;

use crate::desk::book::{self, CoinPosition, Holding};
use crate::desk::hedge::{HedgeCommand, HedgeEvent, HedgeOrder};
use crate::desk::model::MarketModel;
use crate::desk::CuratorRefs;

use super::{ExitsParams, ALERT_ID};

pub use desk_core::exits::put::*;

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
