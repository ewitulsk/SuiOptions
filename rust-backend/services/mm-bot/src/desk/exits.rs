//! The §5 exit ladder, run per tick over the book's held calls:
//!
//!   1. **Resale** (taker only): if the option pool's best bid ≥ model
//!      fair − a small vol-pt concession, sell into it — wallet-held
//!      coins only; vault-held coins are logged as pending adapter
//!      support (TODO(SO-299): curator resale adapter).
//!   2. **Hold** — the default; gamma scalping monetizes.
//!   3. **Exercise** when optimal — `forgone_carry > remaining_time_value
//!      × carry_mult` or near-expiry ITM. Wallet cash first; else
//!      FLASH-EXERCISE via the DeepBook flash-loan PTB
//!      (`sui_tx::tx::deepbook::flash_exercise_call`): borrow strike cost
//!      → `bucket::exercise` → swap underlying → repay, dev-inspect
//!      pre-simulated, aborted if net ≤ 0. Big sizes ladder into
//!      `max_slice` chunks.
//!
//! Puts: exits for held puts are TODO(SO-299) — V1 flow is call-centric
//! (the vault slices are covered calls); held puts just hold to expiry.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use parking_lot::RwLock;
use serde::Deserialize;
use sui_types::base_types::ObjectID;

use pyth_client::{PriceCache, PriceFeedId};
use sui_tx::sui_client::{Network, SuiClientWrapper};
use sui_tx::tx::deepbook::{flash_exercise_call, top_of_book, DeepBookHandles, FlashExerciseCallParams};

use crate::pricing::{compute_spot_from_cache, Staleness};

use super::book::{Book, Holding};
use super::model::MarketModel;

const ALERT_ID: &str = "tx-failed-mm-bot-desk";

/// `[desk.exits]` knobs. Defaults per 00-plan §5.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ExitsConfig {
    pub enabled: bool,
    pub tick_secs: u64,
    /// Resale concession off model fair, vol points.
    pub concession_volpts: f64,
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
            concession_volpts: 1.0,
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
    /// Sell into the option pool's standing bid (taker).
    Resale,
    Hold,
    /// Exercise funded from wallet settlement cash.
    ExerciseCash,
    /// Exercise via the DeepBook flash-loan PTB.
    FlashExercise,
}

/// Pure ladder decision for one held call.
#[allow(clippy::too_many_arguments)]
pub fn decide_exit(
    cfg: &ExitsConfig,
    model: &MarketModel,
    spot: f64,
    strike: f64,
    expiry_ms: u64,
    best_bid_per_unit: Option<f64>,
    wallet_cash: u64,
    strike_cost: u64,
    now_ms: u64,
) -> ExitAction {
    let t = (expiry_ms.saturating_sub(now_ms)) as f64 / 1000.0 / 86_400.0 / 365.0;
    let (sigma, _) = model.sigma(spot, strike, t);
    // (1) resale: standing bid ≥ fair − concession (vol pts → price via
    // vega).
    if let Some(bid) = best_bid_per_unit {
        let fair = model.fair_per_unit(false, spot, strike, t, sigma);
        let vega = model.greeks_per_unit(false, spot, strike, t, sigma).vega;
        let concession = vega * cfg.concession_volpts / 100.0;
        if bid >= fair - concession && bid > 0.0 {
            return ExitAction::Resale;
        }
    }
    // (3) exercise when optimal: forgone carry beats remaining time value
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
    // (2) default: hold and scalp.
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
    /// resale/flash paths; `None` disables them.
    pub deep_coin_type: Option<String>,
    pub core_package: ObjectID,
    /// All exit proceeds land here (vault-only mandate).
    pub vault_address: sui_types::base_types::SuiAddress,
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
    let holdings: Vec<Holding> = p.book.read().holdings.clone();
    let now = super::auctions::now_ms();
    for h in holdings {
        if h.is_put {
            continue; // TODO(SO-299): put exits.
        }
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
        // Option-pool best bid (per unit, settlement-raw per
        // underlying-raw): DeepBook raw price is quote-per-base × 1e9.
        let best_bid = match (&h.pool_id, &p.handles) {
            (Some(pool), Some(handles)) => {
                match ObjectID::from_hex_literal(pool) {
                    Ok(pool_id) => top_of_book(
                        &wrap.client,
                        wrap.signer.address,
                        handles.package,
                        pool_id,
                        &h.option_coin_type,
                        &h.settlement_coin_type,
                    )
                    .await
                    .ok()
                    .and_then(|t| t.best_bid_raw)
                    .map(|raw| raw as f64 / 1e9),
                    Err(_) => None,
                }
            }
            _ => None,
        };
        let wallet_cash = wrap
            .client
            .coin_read_api()
            .get_balance(wrap.signer.address, Some(p.settlement_coin_type.clone()))
            .await
            .map(|b| u64::try_from(b.total_balance).unwrap_or(u64::MAX))
            .unwrap_or(0);
        let exercisable = h.amount_wallet; // vault-held: adapter pending
        let cost_all = strike_cost(exercisable, h.strike, h.strike_scale);
        let action = decide_exit(
            &p.cfg,
            &p.models[mi],
            spot,
            h.strike_scaled(),
            h.expiry_ms,
            best_bid,
            wallet_cash,
            cost_all,
            now,
        );
        metrics::counter!("mm_desk_exit_decisions_total", "action" => match action {
            ExitAction::Resale => "resale",
            ExitAction::Hold => "hold",
            ExitAction::ExerciseCash => "exercise_cash",
            ExitAction::FlashExercise => "flash_exercise",
        })
        .increment(1);
        if action == ExitAction::Hold {
            continue;
        }
        if h.amount_wallet == 0 {
            // Vault-custody coins: no curator exit adapter yet.
            tracing::info!(
                bucket = %h.bucket_id.to_hex(),
                ?action,
                vault_held = h.amount_vault,
                "exit wanted but coins are in vault custody — pending adapter support (TODO SO-299)"
            );
            continue;
        }
        let res = match action {
            ExitAction::Resale => resell(p, wrap, &h, best_bid.unwrap_or(0.0)).await,
            ExitAction::ExerciseCash => exercise_cash(p, wrap, &h, exercisable).await,
            ExitAction::FlashExercise => flash_exercise(p, wrap, &h, spot).await,
            ExitAction::Hold => unreachable!(),
        };
        if let Err(e) = res {
            tracing::error!(
                alert_id = ALERT_ID,
                bucket = %h.bucket_id.to_hex(),
                ?action,
                error = %format!("{e:#}"),
                "exit execution tx failed"
            );
        }
    }
    Ok(())
}

/// Taker-sell wallet-held option coins into the pool's standing bid via
/// the coin-based `swap_exact_base_for_quote` (no BalanceManager needed).
async fn resell(p: &ExitsParams, wrap: &SuiClientWrapper, h: &Holding, bid_per_unit: f64) -> Result<()> {
    let handles = p.handles.as_ref().context("no deepbook handles")?;
    let deep = p.deep_coin_type.as_deref().context("no deep coin type")?;
    let pool = ObjectID::from_hex_literal(h.pool_id.as_deref().unwrap_or_default())?;
    // Accept up to 2% below the observed bid (partial-depth safety).
    let min_out = ((bid_per_unit * h.amount_wallet as f64) * 0.98) as u64;
    let resp = sui_tx::tx::deepbook::swap_base_for_quote(
        &wrap.client,
        &wrap.signer,
        handles.package,
        pool,
        &h.option_coin_type,
        &h.settlement_coin_type,
        deep,
        h.amount_wallet,
        min_out,
        p.vault_address,
        p.cfg.gas_budget,
    )
    .await?;
    tracing::info!(
        bucket = %h.bucket_id.to_hex(),
        amount = h.amount_wallet,
        min_out,
        digest = %resp.digest,
        "resold option coins into pool bid (proceeds → vault)"
    );
    Ok(())
}

/// Exercise funded from wallet cash: gather strike cost → `bucket::exercise`
/// → underlying to the vault.
async fn exercise_cash(p: &ExitsParams, wrap: &SuiClientWrapper, h: &Holding, amount: u64) -> Result<()> {
    use move_core_types::identifier::Identifier;
    use move_core_types::language_storage::TypeTag;
    use std::str::FromStr;
    use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;

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
    let bucket_arg = pt.obj(sui_tx::tx::shared_object_arg(&wrap.client, bucket, true).await?)?;
    let clock = sui_tx::tx::clock_arg(&mut pt)?;
    let tags = vec![
        TypeTag::from_str(&h.asset_coin_type)?,
        TypeTag::from_str(&h.settlement_coin_type)?,
        TypeTag::from_str(&h.option_coin_type)?,
    ];
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
    let resp = sui_tx::tx::submit_ptb(&wrap.client, &wrap.signer, pt, p.cfg.gas_budget, "desk exercise").await?;
    tracing::info!(
        bucket = %h.bucket_id.to_hex(),
        amount,
        cost,
        digest = %resp.digest,
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
            digest = %resp.digest,
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
