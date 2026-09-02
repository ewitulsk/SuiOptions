//! The §5 exit ladder, run per tick over the book's held options — the
//! kernel's `ExitTimer` adapter (SO-450): the tick feeds fresh spots and
//! the wallet's cash to the kernel, which runs the ladder DECISION
//! (`desk_core::exits`, re-exported here) and answers with PTB commands
//! this module executes:
//!
//!   0. **Offset close** (netting, config-gated): when the book holds a
//!      written `Position` and VaultMm-custodied option coins in the SAME
//!      bucket, net them via `vault_mm::close_offset_position` (put twin
//!      for puts) — frees collateral at zero market impact, so it beats
//!      every other rung.
//!   1. **Hold** — the default; gamma scalping monetizes. RESALE is no
//!      longer a rung here: the listings engine (SO-416,
//!      `desk::listings`) rests standing ASKS on the in-house exchange
//!      and its matching engine executes resales whenever a bid crosses,
//!      replacing the retired per-option DeepBook taker swap.
//!   2. **Exercise** when optimal. Wallet coins: wallet cash first, else
//!      FLASH-EXERCISE via the DeepBook flash-loan PTB against the
//!      UNDERLYING/SETTLEMENT spot pool
//!      (`sui_tx::tx::deepbook::flash_exercise_call`). VAULT coin-custody
//!      positions: `vault_mm::exercise_call_coin` (vault free settlement
//!      pays the strike) — skipped when the vault's free settlement
//!      doesn't cover it (the flash fallback is wallet-only).
//!
//! Puts: offset closes work; put EXERCISE (SO-443) runs the three-path
//! atomic waterfall in [`put`] — own/vault underlying → base flash →
//! quote flash — gated on the §0.4 minimum profit, laddered inside
//! expiry, with the LONG-perp hedge close scheduled right after each
//! slice. The near-expiry rung doubles as the redundant keeper sweep:
//! every ITM put inside the window is attempted before expiry.
//!
//! Vault free-balance coins (auction-win redemptions) cannot be
//! exercised: `exercise_call_coin` takes a coin-custody POSITION and
//! there is no re-custody entry — they hold to expiry when exercise is
//! the chosen rung. Units the listings engine has committed to resting
//! asks ([`Book::listed_units`]) are excluded from the vault leg.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use desk_core::kernel::{CallLeg, Command, DeskKernel, Event};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use parking_lot::RwLock;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::Argument;

use protocol_types::ids::ObjectId;
use pyth_client::{PriceCache, PriceFeedId};
use sui_tx::sui_client::{Network, SuiClientWrapper};
use sui_tx::tx::deepbook::{flash_exercise_call, DeepBookHandles, FlashExerciseCallParams};
use sui_tx::tx::{clock_arg, owned_object_arg, shared_object_arg, submit_ptb};

use crate::pricing::{compute_spot_from_cache, Staleness};

use super::book::{self, Holding};
use super::CuratorRefs;

pub use desk_core::exits::*;

pub mod put;

pub(crate) const ALERT_ID: &str = "tx-failed-mm-bot-desk";

// ── the exits task ─────────────────────────────────────────────────────

pub struct ExitsParams {
    pub cfg: ExitsConfig,
    pub secrets: runtime_config::Secrets,
    pub network: Network,
    /// The kernel: the book, the risk gate and the ladder decision.
    pub kernel: Arc<RwLock<DeskKernel>>,
    pub market_feeds: Vec<(PriceFeedId, u8)>,
    pub price_cache: PriceCache,
    pub settlement_feed: PriceFeedId,
    pub settlement_coin_type: String,
    pub settlement_decimals: u8,
    pub staleness: Staleness,
    pub handles: Option<DeepBookHandles>,
    /// The deployment's DEEP token type (swap fee legs). Required for the
    /// wallet flash-exercise path; `None` disables it.
    pub deep_coin_type: Option<String>,
    pub core_package: ObjectID,
    /// All wallet-side exit proceeds land here (vault-only mandate).
    pub vault_address: sui_types::base_types::SuiAddress,
    /// Curator-session refs — `None` disables the vault-custody paths
    /// (offset closes, vault exercise).
    pub curator: Option<CuratorRefs>,
    /// deepbook-adapter package + `PoolAllowlist` — `None` disables the
    /// vault-custody put repurchase (SO-443).
    pub deepbook_adapter: Option<put::AdapterRefs>,
    /// Per-market PRIMARY hedge venue, aligned with the kernel's models;
    /// put exercise schedules its hedge close here.
    pub hedge_venues: Vec<Arc<dyn super::hedge::HedgeVenue>>,
}

pub fn spawn_exits(p: ExitsParams) {
    if !p.cfg.enabled {
        return;
    }
    tokio::spawn(async move {
        let wrap = match SuiClientWrapper::connect(&p.secrets, p.network).await {
            Ok(w) => w,
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "exits: sui connect failed; task exiting");
                return;
            }
        };
        let mut ticker = tokio::time::interval(Duration::from_secs(p.cfg.tick_secs.max(30)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if let Err(e) = tick(&p, &wrap).await {
                tracing::warn!(error = %format!("{e:#}"), "exits tick errored");
            }
        }
    });
}

async fn tick(p: &ExitsParams, wrap: &SuiClientWrapper) -> Result<()> {
    let now = super::auctions::now_ms();
    // Fresh spot per market → the kernel (a stale market's holdings are
    // skipped, as before).
    {
        let mut k = p.kernel.write();
        for (mi, (feed, decimals)) in p.market_feeds.iter().enumerate() {
            match compute_spot_from_cache(
                &p.price_cache,
                *feed,
                p.settlement_feed,
                *decimals,
                p.settlement_decimals,
                p.staleness,
            ) {
                Ok(spot) => k.on_event(Event::Spot { market: mi, spot, at_ms: now }),
                Err(_) => k.on_event(Event::SpotStale { market: mi, at_ms: now }),
            };
        }
    }
    let wallet_cash = match sui_types::parse_sui_struct_tag(&p.settlement_coin_type) {
        Ok(tag) => wrap
            .client
            .balance(wrap.signer.address, &tag)
            .await
            .map(|b| u64::try_from(b).unwrap_or(u64::MAX))
            .unwrap_or(0),
        Err(_) => 0,
    };
    let cmds = p.kernel.write().on_event(Event::ExitTimer { wallet_cash, at_ms: now });
    execute(p, wrap, cmds, now).await
}

/// The held line a command refers to, as the book holds it now.
fn holding(p: &ExitsParams, bucket: &ObjectId) -> Option<Holding> {
    p.kernel.read().book.holdings.iter().find(|h| h.bucket_id == *bucket).cloned()
}

/// Execute the ladder's PTB commands, in order.
async fn execute(p: &ExitsParams, wrap: &SuiClientWrapper, cmds: Vec<Command>, now: u64) -> Result<()> {
    for cmd in cmds {
        match cmd {
            Command::OffsetClose { bucket, position_id, coin_position_id, amount, .. } => {
                let (Some(refs), Some(h)) = (p.curator.as_ref(), holding(p, &bucket)) else {
                    continue;
                };
                match offset_close(p, wrap, refs, &h, position_id, coin_position_id, amount).await {
                    Ok(()) => {
                        metrics::counter!("mm_desk_exit_decisions_total", "action" => "offset_close")
                            .increment(1);
                    }
                    Err(e) => tracing::error!(
                        alert_id = ALERT_ID,
                        bucket = %bucket.to_hex(),
                        amount,
                        error = %format!("{e:#}"),
                        "offset close tx failed"
                    ),
                }
            }
            Command::ExecuteCallPtb { bucket, leg } => {
                let Some(h) = holding(p, &bucket) else { continue };
                match leg {
                    // Wallet leg (float coins: auction remnants / staged exits).
                    CallLeg::WalletCash { amount, .. } => {
                        metrics::counter!("mm_desk_exit_decisions_total", "action" => "exercise_cash")
                            .increment(1);
                        if let Err(e) = exercise_cash(p, wrap, &h, amount).await {
                            tracing::error!(
                                alert_id = ALERT_ID,
                                bucket = %bucket.to_hex(),
                                action = "exercise_cash",
                                error = %format!("{e:#}"),
                                "exit execution tx failed (wallet leg)"
                            );
                        }
                    }
                    CallLeg::WalletFlash { .. } => {
                        metrics::counter!("mm_desk_exit_decisions_total", "action" => "flash_exercise")
                            .increment(1);
                        if let Err(e) = flash_exercise(p, wrap, &h).await {
                            tracing::error!(
                                alert_id = ALERT_ID,
                                bucket = %bucket.to_hex(),
                                action = "flash_exercise",
                                error = %format!("{e:#}"),
                                "exit execution tx failed (wallet leg)"
                            );
                        }
                    }
                    // Vault leg (coin-custody positions).
                    CallLeg::VaultCoins => {
                        let Some(refs) = p.curator.as_ref() else {
                            tracing::info!(
                                bucket = %bucket.to_hex(),
                                "vault-custody exit wanted but curator refs unresolved; holding"
                            );
                            continue;
                        };
                        if let Err(e) = vault_exercise(p, wrap, refs, &h).await {
                            tracing::error!(
                                alert_id = ALERT_ID,
                                bucket = %bucket.to_hex(),
                                action = "vault_exercise",
                                error = %format!("{e:#}"),
                                "exit execution tx failed (vault leg)"
                            );
                        }
                    }
                }
            }
            Command::QueryPutLiquidity { bucket, market } => {
                let Some(h) = holding(p, &bucket) else { continue };
                metrics::counter!("mm_desk_exit_decisions_total", "action" => "exercise_put")
                    .increment(1);
                // Both legs + laddering + hedge close live in the put
                // module; every tx failure is alert-logged at its handler
                // there.
                let res = match put::query_liquidity(p, wrap, &h, market, now).await {
                    Ok(ev) => {
                        let plans = p.kernel.write().on_event(ev);
                        put::execute(p, wrap, &h, market, plans, now).await
                    }
                    Err(e) => Err(e),
                };
                match res {
                    Ok(0) => {}
                    Ok(units) => {
                        metrics::counter!("mm_desk_put_exercised_units_total").increment(units)
                    }
                    Err(e) => tracing::error!(
                        alert_id = ALERT_ID,
                        bucket = %bucket.to_hex(),
                        error = %format!("{e:#}"),
                        "put exercise failed before any tx"
                    ),
                }
            }
            _ => {}
        }
    }
    Ok(())
}

// ── curator-session PTB legs (vault custody) ───────────────────────────

/// The `(vault, cap, reg)` prefix every `vault_mm` / adapter call takes.
/// Fetched fresh per PTB — each submit bumps the owned CuratorCap's
/// version, so its object ref can't be reused across transactions.
async fn curator_args(
    wrap: &SuiClientWrapper,
    refs: &CuratorRefs,
    pt: &mut ProgrammableTransactionBuilder,
) -> Result<(Argument, Argument, Argument)> {
    let vault = pt.obj(shared_object_arg(&wrap.client, refs.vault_id, true).await?)?;
    let cap = pt.obj(owned_object_arg(&wrap.client, refs.curator_cap).await?)?;
    let reg = pt.obj(shared_object_arg(&wrap.client, refs.integration_registry, false).await?)?;
    Ok((vault, cap, reg))
}

fn bucket_type_tags(h: &Holding) -> Result<Vec<TypeTag>> {
    Ok(vec![
        TypeTag::from_str(&h.asset_coin_type)?,
        TypeTag::from_str(&h.settlement_coin_type)?,
        TypeTag::from_str(&h.option_coin_type)?,
    ])
}

/// Step 0: `vault_mm::close_offset_position` (put twin for puts) — net
/// `amount` of a custodied option-coin position against the written
/// `Position`, freeing its collateral into vault balances.
async fn offset_close(
    p: &ExitsParams,
    wrap: &SuiClientWrapper,
    refs: &CuratorRefs,
    h: &Holding,
    position_id: ObjectId,
    coin_position_id: ObjectId,
    amount: u64,
) -> Result<()> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let (vault, cap, reg) = curator_args(wrap, refs, &mut pt).await?;
    let bucket = pt.obj(
        shared_object_arg(&wrap.client, ObjectID::new(*h.bucket_id.as_bytes()), true).await?,
    )?;
    let position_arg = pt.pure(&ObjectID::new(*position_id.as_bytes()))?;
    let coin_position_arg = pt.pure(&ObjectID::new(*coin_position_id.as_bytes()))?;
    let amount_arg = pt.pure(&amount)?;
    let clock = clock_arg(&mut pt)?;
    let function = if h.is_put { "close_offset_put_position" } else { "close_offset_position" };
    pt.programmable_move_call(
        refs.trading_vault_package,
        Identifier::new("vault_mm").unwrap(),
        Identifier::new(function).unwrap(),
        bucket_type_tags(h)?,
        vec![vault, cap, reg, bucket, position_arg, coin_position_arg, amount_arg, clock],
    );
    let resp =
        submit_ptb(&wrap.client, &wrap.signer, pt, p.cfg.gas_budget, "desk offset close").await?;
    tracing::info!(
        bucket = %h.bucket_id.to_hex(),
        position = %position_id.to_hex(),
        coin_position = %coin_position_id.to_hex(),
        amount,
        digest = %sui_tx::tx::tx_digest(&resp),
        "offset-closed written position against held coins (collateral → vault)"
    );
    Ok(())
}

/// Vault exercise: `vault_mm::exercise_call_coin` per coin-custody
/// position, batched into one PTB, gated on the vault's FREE settlement
/// covering the cumulative strike cost (no flash path for vault coins).
async fn vault_exercise(
    p: &ExitsParams,
    wrap: &SuiClientWrapper,
    refs: &CuratorRefs,
    h: &Holding,
) -> Result<()> {
    if h.coin_positions.is_empty() {
        // Free-balance coins have no exercise entry (module docs).
        tracing::info!(
            bucket = %h.bucket_id.to_hex(),
            free_balance = h.amount_vault,
            "vault exercise wanted but coins are free balances (no coin-custody position); holding"
        );
        return Ok(());
    }
    let free_settlement = book::free_balance_of(
        wrap,
        refs.trading_vault_package,
        refs.vault_id,
        &p.settlement_coin_type,
    )
    .await
    .unwrap_or(0);
    let mut budget = free_settlement;
    let mut batch: Vec<&book::CoinPosition> = Vec::new();
    for cp in &h.coin_positions {
        let cost = strike_cost(cp.amount, h.strike, h.strike_scale);
        if cost <= budget {
            budget -= cost;
            batch.push(cp);
        }
    }
    if batch.is_empty() {
        tracing::info!(
            bucket = %h.bucket_id.to_hex(),
            free_settlement,
            needed = strike_cost(h.coin_positions[0].amount, h.strike, h.strike_scale),
            "vault exercise wanted but free settlement doesn't cover the strike; holding"
        );
        return Ok(());
    }
    let mut pt = ProgrammableTransactionBuilder::new();
    let (vault, cap, reg) = curator_args(wrap, refs, &mut pt).await?;
    let bucket = pt.obj(
        shared_object_arg(&wrap.client, ObjectID::new(*h.bucket_id.as_bytes()), true).await?,
    )?;
    let clock = clock_arg(&mut pt)?;
    let tags = bucket_type_tags(h)?;
    let mut total = 0u64;
    for cp in &batch {
        let coin_position_id = pt.pure(&ObjectID::new(*cp.position_id.as_bytes()))?;
        let amount_arg = pt.pure(&cp.amount)?;
        pt.programmable_move_call(
            refs.trading_vault_package,
            Identifier::new("vault_mm").unwrap(),
            Identifier::new("exercise_call_coin").unwrap(),
            tags.clone(),
            vec![vault, cap, reg, bucket, coin_position_id, amount_arg, clock],
        );
        total += cp.amount;
    }
    let resp =
        submit_ptb(&wrap.client, &wrap.signer, pt, p.cfg.gas_budget, "desk vault exercise").await?;
    tracing::info!(
        bucket = %h.bucket_id.to_hex(),
        amount = total,
        positions = batch.len(),
        cost = free_settlement - budget,
        digest = %sui_tx::tx::tx_digest(&resp),
        "exercised vault-custody coins with vault cash (underlying → vault balances)"
    );
    Ok(())
}

// ── wallet legs (float coins) ──────────────────────────────────────────

/// Exercise funded from wallet cash: gather strike cost → `bucket::exercise`
/// → underlying to the vault.
async fn exercise_cash(p: &ExitsParams, wrap: &SuiClientWrapper, h: &Holding, amount: u64) -> Result<()> {
    let bucket = ObjectID::new(*h.bucket_id.as_bytes());
    let cost = strike_cost(amount, h.strike, h.strike_scale);
    let mut pt = ProgrammableTransactionBuilder::new();
    let calls = sui_tx::tx::deepbook::gather_exact_coin(
        &wrap.client,
        &wrap.signer,
        &mut pt,
        &h.option_coin_type,
        amount,
    )
    .await?;
    let payment = sui_tx::tx::deepbook::gather_exact_coin(
        &wrap.client,
        &wrap.signer,
        &mut pt,
        &h.settlement_coin_type,
        cost,
    )
    .await?;
    let bucket_arg = pt.obj(shared_object_arg(&wrap.client, bucket, true).await?)?;
    let clock = clock_arg(&mut pt)?;
    let tags = bucket_type_tags(h)?;
    let underlying = pt.programmable_move_call(
        p.core_package,
        Identifier::new("bucket").unwrap(),
        Identifier::new("exercise").unwrap(),
        tags,
        vec![bucket_arg, calls, payment, clock],
    );
    let recipient = pt.pure(p.vault_address)?;
    pt.command(sui_types::transaction::Command::TransferObjects(
        vec![underlying],
        recipient,
    ));
    let resp = submit_ptb(&wrap.client, &wrap.signer, pt, p.cfg.gas_budget, "desk exercise").await?;
    tracing::info!(
        bucket = %h.bucket_id.to_hex(),
        amount,
        cost,
        digest = %sui_tx::tx::tx_digest(&resp),
        "exercised with wallet cash (underlying → vault)"
    );
    Ok(())
}

/// Flash-exercise: laddered `flash_exercise_call` PTBs, each pre-simulated
/// and aborted if net ≤ 0 inside the builder.
async fn flash_exercise(p: &ExitsParams, wrap: &SuiClientWrapper, h: &Holding) -> Result<()> {
    let handles = p.handles.as_ref().context("no deepbook handles")?;
    let deep = p.deep_coin_type.as_deref().context("no deep coin type")?;
    let symbol = p
        .kernel
        .read()
        .models
        .iter()
        .find(|m| m.coin_type == h.asset_coin_type)
        .map(|m| m.symbol.clone())
        .context("no model for underlying")?;
    let Some(spot_pool) = p.cfg.spot_pools.get(&symbol) else {
        tracing::warn!(
            symbol = %symbol,
            "flash-exercise wanted but no [desk.exits.spot_pools] entry; holding"
        );
        return Ok(());
    };
    let spot_pool = ObjectID::from_hex_literal(spot_pool)?;
    let mut remaining = h.amount_wallet;
    while remaining > 0 {
        let slice = remaining.min(p.cfg.max_slice.max(1));
        let cost = strike_cost(slice, h.strike, h.strike_scale);
        let params = FlashExerciseCallParams {
            deepbook_package: handles.package,
            core_package: p.core_package,
            spot_pool,
            bucket: ObjectID::new(*h.bucket_id.as_bytes()),
            underlying_type: &h.asset_coin_type,
            settlement_type: &h.settlement_coin_type,
            call_coin_type: &h.option_coin_type,
            deep_coin_type: deep,
            amount: slice,
            strike_cost: cost,
            recipient: p.vault_address,
            gas_budget: p.cfg.gas_budget,
        };
        let resp = flash_exercise_call(&wrap.client, &wrap.signer, &params).await?;
        tracing::info!(
            bucket = %h.bucket_id.to_hex(),
            slice,
            cost,
            digest = %sui_tx::tx::tx_digest(&resp),
            "flash-exercised (net proceeds → vault)"
        );
        remaining -= slice;
    }
    Ok(())
}
