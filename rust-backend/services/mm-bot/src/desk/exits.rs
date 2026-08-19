//! The §5 exit ladder, run per tick over the book's held options:
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
//!   2. **Exercise** when optimal — `forgone_carry > remaining_time_value
//!      × carry_mult` or near-expiry ITM. Wallet coins: wallet cash first,
//!      else FLASH-EXERCISE via the DeepBook flash-loan PTB against the
//!      UNDERLYING/SETTLEMENT spot pool
//!      (`sui_tx::tx::deepbook::flash_exercise_call`). VAULT coin-custody
//!      positions: `vault_mm::exercise_call_coin` (vault free settlement
//!      pays the strike) — skipped when the vault's free settlement
//!      doesn't cover it (the flash fallback is wallet-only).
//!
//! Puts: offset closes work; put EXERCISE is TODO(SO-299) — held puts
//! otherwise hold to expiry (their resale channel is the listings
//! engine's resting asks).
//!
//! Vault free-balance coins (auction-win redemptions) cannot be
//! exercised: `exercise_call_coin` takes a coin-custody POSITION and
//! there is no re-custody entry — they hold to expiry when exercise is
//! the chosen rung. Units the listings engine has committed to resting
//! asks ([`Book::listed_units`]) are excluded from the vault leg.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use parking_lot::RwLock;
use serde::Deserialize;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::Argument;

use pyth_client::{PriceCache, PriceFeedId};
use sui_tx::sui_client::{Network, SuiClientWrapper};
use sui_tx::tx::deepbook::{flash_exercise_call, DeepBookHandles, FlashExerciseCallParams};
use sui_tx::tx::{clock_arg, owned_object_arg, shared_object_arg, submit_ptb};

use crate::pricing::{compute_spot_from_cache, Staleness};

use super::book::{self, Book, CoinPosition, Holding, Written};
use super::model::MarketModel;
use super::{CuratorRefs, DeskShared};

const ALERT_ID: &str = "tx-failed-mm-bot-desk";

/// `[desk.exits]` knobs. Defaults per 00-plan §5.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ExitsConfig {
    pub enabled: bool,
    pub tick_secs: u64,
    /// Step 0: net written positions against same-bucket VaultMm coin
    /// custody (`close_offset_*`) before any exercise.
    pub offset_close_enabled: bool,
    /// Exercise when `forgone_carry > remaining_time_value × this`.
    /// 00-plan: 1.1.
    pub carry_mult: f64,
    /// Force-exercise ITM holdings inside this many hours to expiry.
    pub near_expiry_hours: f64,
    /// Flash-exercise ladder chunk, underlying units per tx.
    pub max_slice: u64,
    /// Underlying-symbol → UNDERLYING/SETTLEMENT spot pool id (the
    /// flash-loan + sale venue). Missing entry ⇒ no flash path for that
    /// underlying (cash exercise only).
    pub spot_pools: HashMap<String, String>,
    pub gas_budget: u64,
}

impl Default for ExitsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tick_secs: 300,
            offset_close_enabled: true,
            carry_mult: 1.1,
            near_expiry_hours: 24.0,
            max_slice: 1_000_000_000,
            spot_pools: HashMap::new(),
            gas_budget: 200_000_000,
        }
    }
}

/// What the ladder decided for one holding this tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitAction {
    Hold,
    /// Exercise funded from wallet settlement cash.
    ExerciseCash,
    /// Exercise via the DeepBook flash-loan PTB.
    FlashExercise,
}

/// Pure ladder decision for one held option. Puts only ever hold — put
/// exercise is TODO(SO-299). (Resale is not decided here: the listings
/// engine keeps a standing ask resting on the exchange instead.)
#[allow(clippy::too_many_arguments)]
pub fn decide_exit(
    cfg: &ExitsConfig,
    model: &MarketModel,
    is_put: bool,
    spot: f64,
    strike: f64,
    expiry_ms: u64,
    wallet_cash: u64,
    strike_cost: u64,
    now_ms: u64,
) -> ExitAction {
    let t = (expiry_ms.saturating_sub(now_ms)) as f64 / 1000.0 / 86_400.0 / 365.0;
    let (sigma, _) = model.sigma(spot, strike, t);
    if is_put {
        return ExitAction::Hold; // TODO(SO-299): put exercise.
    }
    // Exercise when optimal: forgone carry beats remaining time value
    // with margin, or near-expiry ITM.
    let itm = spot > strike;
    let near_expiry = (expiry_ms.saturating_sub(now_ms)) as f64 / 3_600_000.0
        <= cfg.near_expiry_hours;
    let carry = model.forgone_carry(spot, strike, t, sigma);
    let tv = model.remaining_time_value_call(spot, strike, t, sigma);
    let exercise = (itm && near_expiry) || (itm && carry > tv * cfg.carry_mult);
    if exercise {
        return if wallet_cash >= strike_cost {
            ExitAction::ExerciseCash
        } else {
            ExitAction::FlashExercise
        };
    }
    // Default: hold and scalp (resting exchange asks handle resale).
    ExitAction::Hold
}

/// `ceil`-free mirror of the bucket's `apply_strike` (round-half-up).
pub fn strike_cost(amount: u64, strike: u128, strike_scale: u8) -> u64 {
    let divisor = 10u128.pow(strike_scale as u32);
    let numerator = amount as u128 * strike;
    u64::try_from((numerator + divisor / 2) / divisor).unwrap_or(u64::MAX)
}

// ── the exits task ─────────────────────────────────────────────────────

pub struct ExitsParams {
    pub cfg: ExitsConfig,
    pub secrets: runtime_config::Secrets,
    pub network: Network,
    /// Shared desk state — the SO-418 risk gate lives here.
    pub shared: Arc<DeskShared>,
    pub book: Arc<RwLock<Book>>,
    pub models: Arc<Vec<MarketModel>>,
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
    let (holdings, written): (Vec<Holding>, Vec<Written>) = {
        let b = p.book.read();
        (b.holdings.clone(), b.written.clone())
    };
    let now = super::auctions::now_ms();
    for h in holdings {
        if h.amount() == 0 || h.expiry_ms <= now {
            continue;
        }
        let Some(mi) = p.models.iter().position(|m| m.coin_type == h.asset_coin_type) else {
            continue;
        };
        let (feed, decimals) = p.market_feeds[mi];
        let Ok(spot) = compute_spot_from_cache(
            &p.price_cache,
            feed,
            p.settlement_feed,
            decimals,
            p.settlement_decimals,
            p.staleness,
        ) else {
            continue;
        };

        // Step 0 (netting): a written position + same-bucket VaultMm coin
        // custody offset-close at zero market impact. One tx per holding
        // per tick; the custody re-sync picks up the shrunk amounts.
        if p.cfg.offset_close_enabled {
            if let (Some(refs), Some(w), Some(cp)) = (
                p.curator.as_ref(),
                written.iter().find(|w| w.bucket_id == h.bucket_id && w.amount > 0),
                h.coin_positions.first(),
            ) {
                let amount = w.amount.min(cp.amount);
                if let Err(e) = offset_close(p, wrap, refs, &h, w, cp, amount).await {
                    tracing::error!(
                        alert_id = ALERT_ID,
                        bucket = %h.bucket_id.to_hex(),
                        amount,
                        error = %format!("{e:#}"),
                        "offset close tx failed"
                    );
                } else {
                    metrics::counter!("mm_desk_exit_decisions_total", "action" => "offset_close")
                        .increment(1);
                }
                continue; // custody changed under us; re-ladder next tick
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
        let cost_wallet = strike_cost(h.amount_wallet, h.strike, h.strike_scale);
        let action = decide_exit(
            &p.cfg,
            &p.models[mi],
            h.is_put,
            spot,
            h.strike_scaled(),
            h.expiry_ms,
            wallet_cash,
            cost_wallet,
            now,
        );
        metrics::counter!("mm_desk_exit_decisions_total", "action" => match action {
            ExitAction::Hold => "hold",
            ExitAction::ExerciseCash => "exercise_cash",
            ExitAction::FlashExercise => "flash_exercise",
        })
        .increment(1);
        if action == ExitAction::Hold {
            continue;
        }

        // Wallet leg (float coins: auction remnants / staged exits).
        if h.amount_wallet > 0 {
            let res = match action {
                ExitAction::ExerciseCash => exercise_cash(p, wrap, &h, h.amount_wallet).await,
                ExitAction::FlashExercise => flash_exercise(p, wrap, &h, spot).await,
                ExitAction::Hold => unreachable!(),
            };
            if let Err(e) = res {
                tracing::error!(
                    alert_id = ALERT_ID,
                    bucket = %h.bucket_id.to_hex(),
                    ?action,
                    error = %format!("{e:#}"),
                    "exit execution tx failed (wallet leg)"
                );
            }
        }

        // Vault leg (coin-custody positions), minus whatever the listings
        // engine has committed to resting exchange asks.
        let listed = p.book.read().listed_units(&h.bucket_id);
        let vault_units = h
            .amount_vault
            .saturating_add(h.amount_coin_positions())
            .saturating_sub(listed);
        if vault_units == 0 || h.is_put {
            continue; // puts never pick exercise today
        }
        let Some(refs) = p.curator.as_ref() else {
            tracing::info!(
                bucket = %h.bucket_id.to_hex(),
                ?action,
                vault_held = vault_units,
                "vault-custody exit wanted but curator refs unresolved; holding"
            );
            continue;
        };
        // SO-418 risk gate: `exercise_call_coin` spends vault FREE
        // settlement to pay the strike — in risk-off nothing leaves free
        // balances except withdrawal fulfillment, so the session aborts
        // on-chain. Hold instead of burning gas. (Offset closes above
        // keep running: netting is unwinding, and frees collateral INTO
        // the vault.)
        if p.shared.risk_off.load(std::sync::atomic::Ordering::Relaxed) {
            tracing::info!(
                bucket = %h.bucket_id.to_hex(),
                ?action,
                "vault-custody exercise wanted but the vault is risk-off; holding"
            );
            continue;
        }
        let res = vault_exercise(p, wrap, refs, &h).await;
        if let Err(e) = res {
            tracing::error!(
                alert_id = ALERT_ID,
                bucket = %h.bucket_id.to_hex(),
                ?action,
                error = %format!("{e:#}"),
                "exit execution tx failed (vault leg)"
            );
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
    w: &Written,
    cp: &CoinPosition,
    amount: u64,
) -> Result<()> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let (vault, cap, reg) = curator_args(wrap, refs, &mut pt).await?;
    let bucket = pt.obj(
        shared_object_arg(&wrap.client, ObjectID::new(*h.bucket_id.as_bytes()), true).await?,
    )?;
    let position_id = pt.pure(&ObjectID::new(*w.position_id.as_bytes()))?;
    let coin_position_id = pt.pure(&ObjectID::new(*cp.position_id.as_bytes()))?;
    let amount_arg = pt.pure(&amount)?;
    let clock = clock_arg(&mut pt)?;
    let function = if h.is_put { "close_offset_put_position" } else { "close_offset_position" };
    pt.programmable_move_call(
        refs.trading_vault_package,
        Identifier::new("vault_mm").unwrap(),
        Identifier::new(function).unwrap(),
        bucket_type_tags(h)?,
        vec![vault, cap, reg, bucket, position_id, coin_position_id, amount_arg, clock],
    );
    let resp =
        submit_ptb(&wrap.client, &wrap.signer, pt, p.cfg.gas_budget, "desk offset close").await?;
    tracing::info!(
        bucket = %h.bucket_id.to_hex(),
        position = %w.position_id.to_hex(),
        coin_position = %cp.position_id.to_hex(),
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
    let mut batch: Vec<&CoinPosition> = Vec::new();
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
async fn flash_exercise(p: &ExitsParams, wrap: &SuiClientWrapper, h: &Holding, _spot: f64) -> Result<()> {
    let handles = p.handles.as_ref().context("no deepbook handles")?;
    let deep = p.deep_coin_type.as_deref().context("no deep coin type")?;
    let model = p
        .models
        .iter()
        .find(|m| m.coin_type == h.asset_coin_type)
        .context("no model for underlying")?;
    let Some(spot_pool) = p.cfg.spot_pools.get(&model.symbol) else {
        tracing::warn!(
            symbol = %model.symbol,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strike_cost_matches_apply_strike_rounding() {
        // Round-half-up mirror of bucket::apply_strike.
        assert_eq!(strike_cost(10, 100, 0), 1_000);
        assert_eq!(strike_cost(1, 15, 1), 2); // 1.5 rounds up
        assert_eq!(strike_cost(1, 14, 1), 1); // 1.4 rounds down
        assert_eq!(strike_cost(7, 100_000_000, 6), 700);
    }
}
