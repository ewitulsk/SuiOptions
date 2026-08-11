//! The spot-band loop, revived from mm-bot's pre-SO-299 testnet market
//! simulator (SO-296) minus every options-side layer (ask writer, position
//! redemption, noise takers — those stayed behind / died with the reset).
//!
//! Per configured pair, each pass: lazily ensure the DeepBook pool exists
//! (lookup by `PoolCreated` event, else `create_permissionless_pool` for
//! the vendored-DEEP fee), faucet-fund both sides, then cancel + re-quote
//! a post-only bid/ask band around the Pyth cross in one PTB. The band
//! follows the oracle, so the book's mid moves like a real market.

use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use tracing::{error, info, warn};

use sui_tx::sui_client::{Network, SuiClientWrapper};
use sui_tx::tx::deepbook::{
    create_balance_manager, create_pool, derived_pool_params, gather_exact_coin, DeepBookHandles,
};
use sui_tx::tx::{clock_arg, shared_object_arg, submit_ptb};

use crate::liquidity::{wallet_balance, FaucetMinter};
use crate::Config;

#[derive(Debug, Clone)]
pub struct SimToken {
    pub symbol: String,
    pub coin_type: String,
    pub decimals: u8,
    pub feed: Option<protocol_types::PriceFeedId>,
}

/// A lazily-ensured spot pool.
#[derive(Debug, Clone)]
struct SpotPool {
    pool_id: ObjectID,
    base: SimToken,
    quote: SimToken,
}

pub struct SimParams {
    pub cfg: Config,
    pub secrets: runtime_config::Secrets,
    pub network: Network,
    pub handles: DeepBookHandles,
    /// Vendored-DEEP creation fee context for lazy pools.
    pub deep_coin_type: String,
    pub pool_creation_fee: u64,
    /// Faucet source for both sides of every band.
    pub liquidity: FaucetMinter,
    pub price_cache: pyth_client::PriceCache,
    pub staleness: pyth_client::Staleness,
    pub tokens: Vec<SimToken>,
}

/// Run the banding loop forever. Returns only on a setup failure the loop
/// cannot recover from (bad config id, BM creation exhausted).
pub async fn run(p: &SimParams) -> Result<()> {
    let wrap = SuiClientWrapper::connect(&p.secrets, p.network).await?;

    let bm_id = match p.cfg.spot_balance_manager_id.as_deref() {
        Some(id) if !id.is_empty() => ObjectID::from_hex_literal(id)?,
        _ => {
            let mut created = None;
            for attempt in 1..=8u32 {
                match create_balance_manager(&wrap.client, &wrap.signer, &p.handles, p.cfg.gas_budget)
                    .await
                {
                    Ok(id) => {
                        info!(bm = %id, "[sim] created spot BalanceManager — persist as spot_balance_manager_id");
                        created = Some(id);
                        break;
                    }
                    Err(e) => {
                        warn!(attempt, error = %format!("{e:#}"), "[sim] spot BM creation failed; retrying");
                        tokio::time::sleep(Duration::from_secs(15)).await;
                    }
                }
            }
            created.ok_or_else(|| anyhow!("spot BalanceManager creation kept failing"))?
        }
    };

    // Resolve configured pairs against the token catalog once.
    let mut pairs: Vec<(SimToken, SimToken)> = Vec::new();
    for pair in &p.cfg.spot_pairs {
        let Some((b, q)) = pair.split_once('/') else {
            warn!(pair, "[sim] bad spot pair (want BASE/QUOTE symbols)");
            continue;
        };
        let find = |sym: &str| p.tokens.iter().find(|t| t.symbol.eq_ignore_ascii_case(sym)).cloned();
        match (find(b), find(q)) {
            (Some(base), Some(quote)) if base.feed.is_some() && quote.feed.is_some() => {
                pairs.push((base, quote))
            }
            _ => warn!(pair, "[sim] spot pair tokens missing from catalog (or no feed); skipped"),
        }
    }
    if pairs.is_empty() {
        warn!("[sim] no usable spot pairs configured — nothing to band");
        return Ok(());
    }

    let mut pools: Vec<SpotPool> = Vec::new();
    // Transient failures (stale price at boot, faucet gas races) warn and
    // retry; sustained failure means the books sit EMPTY while /health stays
    // green — that hid a never-worked funding bug for weeks (SO-302), so it
    // alerts per tx-alerting.md once it stops looking transient.
    const ALERT_AFTER_CONSECUTIVE: u32 = 5;
    let mut consecutive_failures: u32 = 0;
    loop {
        for (base, quote) in &pairs {
            let known = pools
                .iter()
                .find(|sp| sp.base.coin_type == base.coin_type && sp.quote.coin_type == quote.coin_type)
                .map(|sp| sp.pool_id);
            let pool_id = match known {
                Some(id) => id,
                None => match ensure_spot_pool(p, &wrap, base, quote).await {
                    Ok(Some(id)) => {
                        pools.push(SpotPool {
                            pool_id: id,
                            base: base.clone(),
                            quote: quote.clone(),
                        });
                        id
                    }
                    Ok(None) => continue, // unfunded; retried next pass
                    Err(e) => {
                        warn!(base = %base.symbol, quote = %quote.symbol, error = %format!("{e:#}"), "[sim] spot pool ensure failed");
                        continue;
                    }
                },
            };
            match spot_quote_pass(p, &wrap, bm_id, pool_id, base, quote).await {
                Ok(()) => consecutive_failures = 0,
                Err(e) => {
                    consecutive_failures += 1;
                    if consecutive_failures >= ALERT_AFTER_CONSECUTIVE {
                        error!(
                            alert_id = "tx-failed-market-sim",
                            pool = %pool_id,
                            consecutive_failures,
                            error = %format!("{e:#}"),
                            "[sim] spot quote pass failing repeatedly — books may be empty"
                        );
                    } else {
                        warn!(pool = %pool_id, error = %format!("{e:#}"), "[sim] spot quote pass failed");
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(p.cfg.spot_interval_secs)).await;
    }
}

/// Find the pair's pool by its creation event, else create it. `None` =
/// wallet lacks the vendored-DEEP fee (warned; retried next pass).
async fn ensure_spot_pool(
    p: &SimParams,
    wrap: &SuiClientWrapper,
    base: &SimToken,
    quote: &SimToken,
) -> Result<Option<ObjectID>> {
    let event_type = format!(
        "{}::pool::PoolCreated<{}, {}>",
        p.handles.original_package, base.coin_type, quote.coin_type
    );
    // gRPC has no events query; this discovery read goes over GraphQL.
    if let Ok(page) = wrap
        .events
        .query_by_type(&event_type, None, 1, /* descending */ true)
        .await
    {
        if let Some(ev) = page.data.first() {
            if let Some(id) = ev
                .parsed_json
                .pointer("/pool_id")
                .and_then(|v| v.as_str())
                .and_then(|s| ObjectID::from_hex_literal(s).ok())
            {
                info!(pool = %id, base = %base.symbol, quote = %quote.symbol, "[sim] found existing spot pool");
                return Ok(Some(id));
            }
        }
    }
    // No pool: create it — needs the vendored-DEEP creation fee.
    let deep = wallet_balance(&wrap.client, &wrap.signer, &p.deep_coin_type).await;
    if deep < p.pool_creation_fee {
        warn!(
            base = %base.symbol,
            quote = %quote.symbol,
            need = p.pool_creation_fee,
            have = deep,
            wallet = %wrap.signer.address,
            "[sim] cannot create spot pool: wallet lacks vendored DEEP — \
             transfer the fee from the deployer wallet to enable this pair"
        );
        return Ok(None);
    }
    // Bounded retry, same shape as the BM-creation loop above: the wallet
    // is shared by every deployer-keyed service, so a submit can lose a
    // gas-coin race ("object … unavailable for consumption") transiently —
    // and with `spot_interval_secs` this slow, an unretried loss costs a
    // whole pass interval of missing pool.
    let mut id = None;
    for attempt in 1..=8u32 {
        match create_pool(
            &wrap.client,
            &wrap.signer,
            &p.handles,
            &p.deep_coin_type,
            p.pool_creation_fee,
            &base.coin_type,
            &quote.coin_type,
            base.decimals,
            quote.decimals,
            p.cfg.gas_budget,
        )
        .await
        {
            Ok(created) => {
                id = Some(created);
                break;
            }
            Err(e) => {
                warn!(
                    attempt,
                    base = %base.symbol,
                    quote = %quote.symbol,
                    error = %format!("{e:#}"),
                    "[sim] spot pool creation failed; retrying"
                );
                tokio::time::sleep(Duration::from_secs(15)).await;
            }
        }
    }
    let id = id.ok_or_else(|| anyhow!("spot pool creation kept failing for {}", base.symbol))?;
    info!(
        pool = %id,
        base = %base.symbol,
        quote = %quote.symbol,
        "[sim] created spot pool lazily — have an admin run deepbook_adapter::allow_pool to vet it for vault curators"
    );
    Ok(Some(id))
}

/// One banding pass: fund both sides from the faucet, cancel, re-quote
/// bid/ask around the Pyth cross.
async fn spot_quote_pass(
    p: &SimParams,
    wrap: &SuiClientWrapper,
    bm_id: ObjectID,
    pool_id: ObjectID,
    base: &SimToken,
    quote: &SimToken,
) -> Result<()> {
    let mid = pyth_client::compute_spot_from_cache(
        &p.price_cache,
        base.feed.ok_or_else(|| anyhow!("no base feed"))?,
        quote.feed.ok_or_else(|| anyhow!("no quote feed"))?,
        base.decimals,
        quote.decimals,
        p.staleness,
    )
    .map_err(|e| anyhow!("stale spot for {}/{}: {e:?}", base.symbol, quote.symbol))?;

    let (tick, lot, min) = derived_pool_params(base.decimals, quote.decimals);
    // Makers round AWAY from mid: bid down, ask up. Rounding both down let a
    // low-priced pair (TWAL) collapse the whole band into one tick — bid ==
    // ask, and the post-only ask "crossed" its own bid (order_info abort 5).
    let round_down = |px: f64| -> u64 {
        let raw = (px * 1e9) as u64;
        ((raw / tick).max(1)) * tick
    };
    let round_up = |px: f64| -> u64 {
        let raw = (px * 1e9) as u64;
        (raw.div_ceil(tick).max(2)) * tick
    };
    let band = p.cfg.spot_band_bps as f64 / 10_000.0;
    let bid_px = round_down(mid * (1.0 - band));
    let mut ask_px = round_up(mid * (1.0 + band));
    if ask_px <= bid_px {
        ask_px = bid_px + tick;
    }
    let qty = {
        let base_units = (p.cfg.spot_notional_per_side as f64 / mid) as u64;
        ((base_units / lot).max(1)) * lot
    }
    .max(min);

    // Fund both sides: quote notional for the bid, base qty for the ask.
    // With pay_with_deep = false the pool escrows the order PLUS the
    // input-token fee (taker_fee × fee_penalty_multiplier, ~12.5 bps at
    // defaults) — funding the exact amount left every ask short and aborted
    // the whole pass with EBalanceManagerBalanceTooLow. 2% headroom covers
    // any governed fee; the surplus stays in the BM for the next pass.
    let with_fee_headroom = |amount: u64| -> u64 { amount + amount / 50 };
    let quote_need =
        with_fee_headroom(((qty as u128 * ask_px as u128) / 1_000_000_000) as u64);
    let base_need = with_fee_headroom(qty);
    let have_q = p
        .liquidity
        .ensure_wallet_balance(&wrap.client, &wrap.signer, &quote.coin_type, quote_need)
        .await;
    let have_b = p
        .liquidity
        .ensure_wallet_balance(&wrap.client, &wrap.signer, &base.coin_type, base_need)
        .await;
    if have_q < quote_need || have_b < base_need {
        return Err(anyhow!("faucet came up short for {}/{}", base.symbol, quote.symbol));
    }
    let mut pt = ProgrammableTransactionBuilder::new();
    let bm = pt.obj(shared_object_arg(&wrap.client, bm_id, true).await?)?;
    let qcoin = gather_exact_coin(&wrap.client, &wrap.signer, &mut pt, &quote.coin_type, quote_need).await?;
    pt.programmable_move_call(
        p.handles.package,
        Identifier::new("balance_manager").unwrap(),
        Identifier::new("deposit").unwrap(),
        vec![TypeTag::from_str(&quote.coin_type)?],
        vec![bm, qcoin],
    );
    let bcoin = gather_exact_coin(&wrap.client, &wrap.signer, &mut pt, &base.coin_type, base_need).await?;
    pt.programmable_move_call(
        p.handles.package,
        Identifier::new("balance_manager").unwrap(),
        Identifier::new("deposit").unwrap(),
        vec![TypeTag::from_str(&base.coin_type)?],
        vec![bm, bcoin],
    );

    // Cancel + requote in the same PTB.
    let pool = pt.obj(shared_object_arg(&wrap.client, pool_id, true).await?)?;
    let proof = pt.programmable_move_call(
        p.handles.package,
        Identifier::new("balance_manager").unwrap(),
        Identifier::new("generate_proof_as_owner").unwrap(),
        vec![],
        vec![bm],
    );
    let tags = vec![TypeTag::from_str(&base.coin_type)?, TypeTag::from_str(&quote.coin_type)?];
    let clock = clock_arg(&mut pt)?;
    pt.programmable_move_call(
        p.handles.package,
        Identifier::new("pool").unwrap(),
        Identifier::new("cancel_all_orders").unwrap(),
        tags.clone(),
        vec![pool, bm, proof, clock],
    );
    // Orders are Good-Till-Time and every pass cancels + re-places, so the
    // expiry must outlive the re-quote interval or the book empties between
    // passes. Two intervals of headroom keeps orders resting even if one pass
    // fails; floored at 10 min so the fast default is unchanged.
    let expire = now_ms() + (p.cfg.spot_interval_secs * 1000 * 2).max(10 * 60 * 1000);
    for (px, is_bid) in [(bid_px, true), (ask_px, false)] {
        let a_client = pt.pure(now_ms() / 60_000)?;
        let a_type = pt.pure(3u8)?; // post-only
        let a_self = pt.pure(0u8)?;
        let a_px = pt.pure(px)?;
        let a_qty = pt.pure(qty)?;
        let a_bid = pt.pure(is_bid)?;
        let a_deep = pt.pure(false)?;
        let a_exp = pt.pure(expire)?;
        pt.programmable_move_call(
            p.handles.package,
            Identifier::new("pool").unwrap(),
            Identifier::new("place_limit_order").unwrap(),
            tags.clone(),
            vec![pool, bm, proof, a_client, a_type, a_self, a_px, a_qty, a_bid, a_deep, a_exp, clock],
        );
    }
    submit_ptb(&wrap.client, &wrap.signer, pt, p.cfg.gas_budget, "sim::spot_quote").await?;
    info!(pool = %pool_id, base = %base.symbol, bid_px, ask_px, qty, "[sim] spot band refreshed");

    // Sweep fill proceeds separately: `withdraw_settled_amounts_permissionless`
    // hard-aborts with ENoBalanceToSettle (7) when nothing has filled, which
    // is every pass on a quiet book — inside the quote PTB it reverted the
    // whole band. Best-effort here; the abort is the benign no-fills case.
    let mut sweep = ProgrammableTransactionBuilder::new();
    let s_pool = sweep.obj(shared_object_arg(&wrap.client, pool_id, true).await?)?;
    let s_bm = sweep.obj(shared_object_arg(&wrap.client, bm_id, true).await?)?;
    sweep.programmable_move_call(
        p.handles.package,
        Identifier::new("pool").unwrap(),
        Identifier::new("withdraw_settled_amounts_permissionless").unwrap(),
        tags,
        vec![s_pool, s_bm],
    );
    if let Err(e) = submit_ptb(&wrap.client, &wrap.signer, sweep, p.cfg.gas_budget, "sim::spot_settle").await {
        tracing::debug!(pool = %pool_id, error = %format!("{e:#}"), "[sim] settled-amount sweep skipped (benign when no fills)");
    }
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
