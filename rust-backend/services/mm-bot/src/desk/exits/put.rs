//! Put exercise EXECUTION (SO-443, doc 08 §4.4): the three-path atomic
//! waterfall and the redundant near-expiry sweep. The policy — timing
//! rule, minimum profit, route choice, ladder — is `desk_core::exits::put`,
//! re-exported here; the kernel plans every slice
//! (`Command::ExecutePutPtb`) from the pool ladder this module reads
//! (`Command::QueryPutLiquidity` → `Event::PutLiquidity`).
//!
//! Every route is pre-simulated with explicit max-input / min-output,
//! exact repayment, the configured pool allowlist, a flash-capacity
//! check, a gas bound and an on-chain minimum-profit assertion; any
//! failure aborts the whole PTB (`sui_tx::tx::put_exercise`).
//!
//! The Sui PTB is atomic; the LONG-perp hedge unwind is not. After a
//! successful slice the kernel schedules the hedge close (a SELL of
//! `|Δ| × units`) and this module sends it through the market's primary
//! [`HedgeVenue`] immediately, logging its delay as its own line.
//!
//! Custody legs mirror the call path: wallet float coins use the wallet
//! routes (own underlying / base flash / quote flash); VaultMm coin-
//! custody positions use `vault_mm::exercise_put_coin` + the
//! deepbook-adapter repurchase on an allowlisted pool (vault-underlying
//! only — flash routes are wallet-only, exactly like calls).

use std::time::Instant;

use anyhow::{Context, Result};
use desk_core::kernel::{Command, Event, PutLeg};
use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::put_exercise::{
    self, PutPtbSpec, PutSubmitRefs, VaultPutPtbArgs, VaultPutPtbSpec,
};
use sui_tx::tx::{clock_arg, shared_object_arg, submit_ptb};
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;

use crate::desk::book::{self, CoinPosition, Holding};
use crate::desk::hedge::{HedgeCommand, HedgeEvent, HedgeOrder};
use crate::desk::CuratorRefs;

use super::{ExitsParams, ALERT_ID};

pub use desk_core::exits::put::*;

// ── execution ──────────────────────────────────────────────────────────

/// The deepbook-adapter package + shared `PoolAllowlist` the vault-custody
/// repurchase needs.
#[derive(Clone, Copy, Debug)]
pub struct AdapterRefs {
    pub package: ObjectID,
    pub pool_allowlist: ObjectID,
}

/// The allowlisted spot pool for the holding's underlying.
fn spot_pool(p: &ExitsParams, mi: usize) -> Result<ObjectID> {
    let symbol = p.kernel.read().models[mi].symbol.clone();
    let pool = p
        .cfg
        .spot_pools
        .get(&symbol)
        .with_context(|| format!("no [desk.exits.spot_pools] entry for {symbol}"))?;
    Ok(ObjectID::from_hex_literal(pool)?)
}

/// Answer the kernel's `QueryPutLiquidity`: the spot pool's flash
/// capacity, lot/min size and ask ladder, plus the underlying each leg
/// already holds (wallet float / vault free balance).
pub async fn query_liquidity(
    p: &ExitsParams,
    wrap: &SuiClientWrapper,
    h: &Holding,
    mi: usize,
    now_ms: u64,
) -> Result<Event> {
    let cfg = &p.cfg.put;
    let pool = spot_pool(p, mi)?;
    let handles = p.handles.as_ref().context("no deepbook handles")?;
    p.deep_coin_type.as_deref().context("no deep coin type")?;
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
    let wallet_underlying = if h.amount_wallet > 0 {
        wallet_balance(wrap, &h.asset_coin_type).await
    } else {
        0
    };
    let vault_underlying = match (p.curator.as_ref(), p.deepbook_adapter.as_ref()) {
        (Some(refs), Some(_)) if h.amount_coin_positions() > 0 => book::free_balance_of(
            wrap,
            refs.trading_vault_package,
            refs.vault_id,
            &h.asset_coin_type,
        )
        .await
        .unwrap_or(0),
        _ => 0,
    };
    Ok(Event::PutLiquidity {
        bucket: h.bucket_id,
        wallet_underlying,
        vault_underlying,
        pool: liq_pool,
        at_ms: now_ms,
    })
}

/// Run the kernel's put slices for one holding: wallet leg (float
/// coins) then vault leg (coin-custody positions). The first failed
/// wallet slice ends the wallet leg; a failed vault slice ends the tick
/// for this holding. Returns the units exercised.
pub async fn execute(
    p: &ExitsParams,
    wrap: &SuiClientWrapper,
    h: &Holding,
    mi: usize,
    cmds: Vec<Command>,
    now_ms: u64,
) -> Result<u64> {
    let cfg = &p.cfg.put;
    let pool = spot_pool(p, mi)?;
    let handles = p.handles.as_ref().context("no deepbook handles")?;
    let deep = p.deep_coin_type.as_deref().context("no deep coin type")?;
    let mut exercised = 0u64;
    let mut wallet_dead = false;
    for cmd in cmds {
        let Command::ExecutePutPtb { leg, plan, .. } = cmd else { continue };
        match leg {
            PutLeg::Wallet => {
                if wallet_dead {
                    continue;
                }
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
                        landed(p, h, mi, leg, plan.amount, now_ms, started).await;
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
                        failed(p, h, leg, plan.amount, now_ms);
                        wallet_dead = true;
                    }
                }
            }
            PutLeg::VaultCoin { position_id } => {
                let (Some(refs), Some(adapter)) = (p.curator.as_ref(), p.deepbook_adapter.as_ref())
                else {
                    continue;
                };
                let Some(cp) = h.coin_positions.iter().find(|c| c.position_id == position_id) else {
                    continue;
                };
                let started = Instant::now();
                match vault_exercise(p, wrap, refs, adapter, h, cp, &plan, pool).await {
                    Ok(()) => {
                        metrics::counter!("mm_desk_put_exercise_total", "path" => "vault_custody")
                            .increment(1);
                        exercised += plan.amount;
                        landed(p, h, mi, leg, plan.amount, now_ms, started).await;
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
                        failed(p, h, leg, plan.amount, now_ms);
                        return Ok(exercised);
                    }
                }
            }
        }
    }
    Ok(exercised)
}

/// A slice landed: tell the kernel, and send the hedge close it
/// schedules through the market's primary venue.
async fn landed(
    p: &ExitsParams,
    h: &Holding,
    mi: usize,
    leg: PutLeg,
    units: u64,
    now_ms: u64,
    ptb_done: Instant,
) {
    let cmds = p.kernel.write().on_event(Event::ExercisePtbResult {
        bucket: h.bucket_id,
        leg,
        units,
        ok: true,
        at_ms: now_ms,
    });
    for cmd in cmds {
        if let Command::SubmitHedgeOrder { order, .. } = cmd {
            hedge_close(p, h, mi, order, ptb_done, now_ms).await;
        }
    }
}

fn failed(p: &ExitsParams, h: &Holding, leg: PutLeg, units: u64, now_ms: u64) {
    p.kernel.write().on_event(Event::ExercisePtbResult {
        bucket: h.bucket_id,
        leg,
        units,
        ok: false,
        at_ms: now_ms,
    });
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

/// Send the kernel's hedge close — a SELL of `|Δ| × units` on the
/// market's primary venue, scheduled immediately after the PTB lands —
/// and hand the venue's events back; the delay from PTB success is its
/// own log line and histogram (doc 08 §4.4: the unwind is not atomic
/// with the chain).
async fn hedge_close(
    p: &ExitsParams,
    h: &Holding,
    mi: usize,
    order: HedgeOrder,
    ptb_done: Instant,
    now_ms: u64,
) {
    let Some(venue) = p.hedge_venues.get(mi) else {
        return;
    };
    let id = order.id;
    let size = order.size_units;
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
                decimals = p.market_feeds[mi].1,
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
            let mut k = p.kernel.write();
            for ev in events {
                k.on_event(Event::Hedge { market: mi, event: ev, at_ms: now_ms });
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
