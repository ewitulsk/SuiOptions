//! Trading-vault DeepBook quoter (SO-291).
//!
//! Same quoting brain as [`crate::deepbook`], but the orders rest in a
//! curated trading vault's DeepBook custody instead of the bot's own
//! BalanceManager: every on-chain action goes through the
//! `deepbook_adapter` curator entry points. Each adapter call opens and
//! closes its own vault session internally, so packing many calls into one
//! PTB is fine — they run as sequential sessions.
//!
//! Prerequisites: the bot's Sui wallet holds the vault's `CuratorCap`
//! (every adapter call takes it by reference), and the custody has been
//! funded from the vault's free balances (the curator's concern, out of
//! band). The quoter places fixed `quote_size` orders and lets an
//! underfunded custody surface as a tx-failure alert rather than modeling
//! custody inventory.
//!
//! Cycle (mirrors `deepbook::cycle`, minus the wallet-inventory machinery
//! that has no vault-mode equivalent): per tick, for each tradeable bucket
//! pool of the configured markets — `withdraw_settled` → `cancel_all_orders`
//! → place bid/ask `place_limit_order`s, batched `max_pools_per_tx` pools
//! per PTB. Cadence / sizing / batching knobs are reused from the
//! `[deepbook]` config section.

use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use serde::Deserialize;
use sui_json_rpc_types::{ObjectChange, SuiTransactionBlockResponse};
use sui_sdk::SuiClient;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{Argument, ObjectArg, SharedObjectMutability};
use sui_types::{SUI_CLOCK_OBJECT_ID, SUI_CLOCK_OBJECT_SHARED_VERSION};

use api_service_client::{ApiServiceClient, TradeableBucket};
use pyth_client::{PriceCache, PriceFeedId};
use sui_tx::sui_client::{Network, Signer, SuiClientWrapper};
use sui_tx::tx::deepbook::{
    QuotePlan, QuoteSide, ORDER_TYPE_POST_ONLY, SELF_MATCHING_CANCEL_TAKER,
};
use sui_tx::tx::{owned_object_arg, shared_object_arg, submit_ptb};

use crate::deepbook::{
    market_spot, now_ms, price_bucket_quote, BucketQuote, DeepBookQuoterConfig, QuoterMarket,
};
use crate::pricing::{PricingConfig, SigmaEstimate, Staleness};

const ALERT_ID: &str = "tx-failed-mm-bot-vault-deepbook";

// -- Config ------------------------------------------------------------------

/// `[trading_vault]` section of the bot config: quote through a curated
/// trading vault's DeepBook custody instead of the bot's own BalanceManager.
/// Off by default; mutually exclusive with `[deepbook]` (vault mode wins if
/// both are enabled). Cadence / sizing / batching knobs are reused from the
/// `[deepbook]` section.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TradingVaultConfig {
    pub enabled: bool,
    /// The shared `TradingVault` object id.
    pub vault_id: String,
    /// The vault's `DeepBookCustody` position id. Leave empty on first run:
    /// the bot calls `init_custody` once at boot and logs the id — persist
    /// it here afterwards.
    pub custody_id: String,
    /// The vault's `CuratorCap` object id — must be owned by the bot's
    /// Sui wallet.
    pub curator_cap_id: String,
}

/// Everything the vault quoter task needs, captured at boot.
pub struct VaultQuoterParams {
    pub cfg: TradingVaultConfig,
    /// Pool / size / cadence knobs shared with the plain quoter.
    pub db_cfg: DeepBookQuoterConfig,
    pub secrets: runtime_config::Secrets,
    pub network: Network,
    /// The `deepbook-adapter` package id (module `deepbook_adapter`).
    pub adapter_package: ObjectID,
    /// Shared `IntegrationRegistry` (immutable in every call).
    pub integration_registry: ObjectID,
    /// Shared `PoolAllowlist` (immutable; place-order calls only).
    pub pool_allowlist: ObjectID,
    pub api_url: String,
    pub price_cache: PriceCache,
    pub markets: Vec<QuoterMarket>,
    pub settlement_feed: PriceFeedId,
    pub settlement_coin_type: String,
    pub settlement_decimals: u8,
    pub pricing: PricingConfig,
    pub staleness: Staleness,
}

// -- Client-order-id convention (SO-294) -------------------------------------

/// Deterministic client-order-id: `(unix_minute << 16) | ((pool_index & 0xff)
/// << 8) | side`, with side `0` = bid, `1` = ask. `unix_minute` is the quote
/// cycle's unix-epoch minute and `pool_index` the pool's index within that
/// cycle's refresh set, so any observer can decode when an order was placed,
/// which pool slot it came from, and which side it is — and the pair of
/// orders from one refresh share a prefix. (The plain quoter's ids are built
/// inside `sui_tx::tx::deepbook`, owned by a parallel workstream, so only
/// vault mode uses this scheme for now.)
pub(crate) fn client_order_id(unix_minute: u64, pool_index: usize, is_ask: bool) -> u64 {
    (unix_minute << 16) | (((pool_index as u64) & 0xff) << 8) | (is_ask as u64)
}

// -- PTB builders ------------------------------------------------------------

/// Resolved on-chain identities for the adapter calls.
struct VaultRefs {
    /// `deepbook-adapter` package id.
    package: ObjectID,
    vault_id: ObjectID,
    curator_cap: ObjectID,
    registry: ObjectID,
    allowlist: ObjectID,
    /// Passed as a pure `ID` argument to every custody-touching call.
    custody_id: ObjectID,
}

fn adapter_call(
    pt: &mut ProgrammableTransactionBuilder,
    package: ObjectID,
    function: &str,
    tags: Vec<TypeTag>,
    args: Vec<Argument>,
) -> Argument {
    pt.programmable_move_call(
        package,
        Identifier::new("deepbook_adapter").unwrap(),
        Identifier::new(function).unwrap(),
        tags,
        args,
    )
}

/// Immutable Clock input (sui-tx's `clock_arg` is `pub(crate)`, so it's
/// mirrored here rather than imported).
fn clock_arg(pt: &mut ProgrammableTransactionBuilder) -> Result<Argument> {
    Ok(pt.obj(ObjectArg::SharedObject {
        id: SUI_CLOCK_OBJECT_ID,
        initial_shared_version: SUI_CLOCK_OBJECT_SHARED_VERSION,
        mutability: SharedObjectMutability::Immutable,
    })?)
}

/// The argument set every adapter call starts with. Fetched fresh per PTB —
/// each submit bumps the owned CuratorCap's version, so its object ref can't
/// be reused across transactions.
struct CommonArgs {
    vault: Argument,
    cap: Argument,
    reg: Argument,
    list: Argument,
    custody: Argument,
    clock: Argument,
}

async fn common_args(
    client: &SuiClient,
    refs: &VaultRefs,
    pt: &mut ProgrammableTransactionBuilder,
) -> Result<CommonArgs> {
    Ok(CommonArgs {
        vault: pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?,
        cap: pt.obj(owned_object_arg(client, refs.curator_cap).await?)?,
        reg: pt.obj(shared_object_arg(client, refs.registry, false).await?)?,
        list: pt.obj(shared_object_arg(client, refs.allowlist, false).await?)?,
        custody: pt.pure(&refs.custody_id)?,
        clock: clock_arg(pt)?,
    })
}

/// `deepbook_adapter::init_custody(vault, cap, reg)` — one-time custody
/// creation. Returns the created `DeepBookCustody`'s id (the vault stores
/// positions as dynamic *object* fields, so the custody shows up in the tx's
/// object changes even though it lands inside the vault).
async fn init_custody(
    client: &SuiClient,
    signer: &Signer,
    refs: &VaultRefs,
    gas_budget: u64,
) -> Result<ObjectID> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(shared_object_arg(client, refs.vault_id, true).await?)?;
    let cap = pt.obj(owned_object_arg(client, refs.curator_cap).await?)?;
    let reg = pt.obj(shared_object_arg(client, refs.registry, false).await?)?;
    adapter_call(&mut pt, refs.package, "init_custody", vec![], vec![vault, cap, reg]);
    let resp = submit_ptb(client, signer, pt, gas_budget, "deepbook-adapter init_custody").await?;
    let suffix = "::deepbook_adapter::DeepBookCustody";
    resp.object_changes
        .unwrap_or_default()
        .into_iter()
        .find_map(|c| match c {
            ObjectChange::Created { object_id, object_type, .. }
                if object_type.to_string().ends_with(suffix) =>
            {
                Some(object_id)
            }
            _ => None,
        })
        .ok_or_else(|| anyhow!("init_custody succeeded but no DeepBookCustody in object changes"))
}

/// One pool's desired resting quote for a batched vault refresh.
struct VaultPoolRefresh {
    pool_id: ObjectID,
    /// Pool base = the bucket's call coin.
    base_coin_type: String,
    /// Pool quote = the settlement asset.
    quote_coin_type: String,
    /// Index within this cycle's refresh set — the middle byte of the
    /// SO-294 client-order-id.
    pool_index: usize,
    plan: QuotePlan,
}

/// Refresh many pools through the adapter in as few transactions as
/// possible. Per pool: `withdraw_settled` → `cancel_all_orders` → place
/// bid/ask (`place_limit_order`); a pool with an empty plan is a
/// cancel-only entry. Every adapter call is a self-contained vault session,
/// so any number of them can share a PTB; pools are packed
/// `max_pools_per_tx` at a time to stay under PTB limits.
async fn refresh_pools_batched(
    client: &SuiClient,
    signer: &Signer,
    refs: &VaultRefs,
    refreshes: &[VaultPoolRefresh],
    unix_minute: u64,
    max_pools_per_tx: usize,
    gas_budget: u64,
) -> Result<Vec<SuiTransactionBlockResponse>> {
    if refreshes.is_empty() {
        return Ok(Vec::new());
    }
    let chunk = max_pools_per_tx.max(1);
    let mut responses = Vec::with_capacity(refreshes.len() / chunk + 1);

    for group in refreshes.chunks(chunk) {
        let mut pt = ProgrammableTransactionBuilder::new();
        let common = common_args(client, refs, &mut pt).await?;

        for r in group {
            let base_tag = TypeTag::from_str(&r.base_coin_type)
                .with_context(|| format!("parsing base type {}", r.base_coin_type))?;
            let quote_tag = TypeTag::from_str(&r.quote_coin_type)
                .with_context(|| format!("parsing quote type {}", r.quote_coin_type))?;
            let pool = pt.obj(shared_object_arg(client, r.pool_id, true).await?)?;

            adapter_call(
                &mut pt,
                refs.package,
                "withdraw_settled",
                vec![base_tag.clone(), quote_tag.clone()],
                vec![common.vault, common.cap, common.reg, common.custody, pool],
            );
            adapter_call(
                &mut pt,
                refs.package,
                "cancel_all_orders",
                vec![base_tag.clone(), quote_tag.clone()],
                vec![common.vault, common.cap, common.reg, common.custody, pool, common.clock],
            );

            let place = |pt: &mut ProgrammableTransactionBuilder,
                         side: QuoteSide,
                         is_bid: bool|
             -> Result<()> {
                let coid = pt.pure(&client_order_id(unix_minute, r.pool_index, !is_bid))?;
                let order_type = pt.pure(&ORDER_TYPE_POST_ONLY)?;
                let self_matching = pt.pure(&SELF_MATCHING_CANCEL_TAKER)?;
                let price = pt.pure(&side.price_raw)?;
                let qty = pt.pure(&side.quantity)?;
                let is_bid_arg = pt.pure(&is_bid)?;
                let pay_with_deep = pt.pure(&false)?;
                let expire = pt.pure(&r.plan.expire_timestamp_ms)?;
                adapter_call(
                    pt,
                    refs.package,
                    "place_limit_order",
                    vec![base_tag.clone(), quote_tag.clone()],
                    vec![
                        common.vault,
                        common.cap,
                        common.reg,
                        common.list,
                        common.custody,
                        pool,
                        coid,
                        order_type,
                        self_matching,
                        price,
                        qty,
                        is_bid_arg,
                        pay_with_deep,
                        expire,
                        common.clock,
                    ],
                );
                Ok(())
            };
            if let Some(bid) = r.plan.bid {
                place(&mut pt, bid, true)?;
            }
            if let Some(ask) = r.plan.ask {
                place(&mut pt, ask, false)?;
            }
        }

        responses.push(submit_ptb(client, signer, pt, gas_budget, "vault-deepbook refresh").await?);
    }
    Ok(responses)
}

/// Cancel everything the custody has resting on `pool` (shutdown /
/// pool-exit path).
async fn cancel_all_on_pool(
    client: &SuiClient,
    signer: &Signer,
    refs: &VaultRefs,
    pool_id: ObjectID,
    base_coin_type: &str,
    quote_coin_type: &str,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    let base_tag = TypeTag::from_str(base_coin_type)?;
    let quote_tag = TypeTag::from_str(quote_coin_type)?;
    let mut pt = ProgrammableTransactionBuilder::new();
    let common = common_args(client, refs, &mut pt).await?;
    let pool = pt.obj(shared_object_arg(client, pool_id, true).await?)?;
    adapter_call(
        &mut pt,
        refs.package,
        "cancel_all_orders",
        vec![base_tag, quote_tag],
        vec![common.vault, common.cap, common.reg, common.custody, pool, common.clock],
    );
    submit_ptb(client, signer, pt, gas_budget, "vault-deepbook cancel").await
}

// -- Quoter task -------------------------------------------------------------

pub fn spawn_quoter(p: VaultQuoterParams) {
    tokio::spawn(async move {
        if let Err(e) = run(p).await {
            tracing::error!(error = %format!("{e:#}"), "trading-vault deepbook quoter exited");
        }
    });
}

async fn run(p: VaultQuoterParams) -> Result<()> {
    let wrap = SuiClientWrapper::connect(&p.secrets, p.network).await?;
    let api = ApiServiceClient::new(&p.api_url);

    let vault_id = ObjectID::from_hex_literal(p.cfg.vault_id.trim())
        .map_err(|e| anyhow!("bad trading_vault.vault_id {}: {e}", p.cfg.vault_id))?;
    let curator_cap = ObjectID::from_hex_literal(p.cfg.curator_cap_id.trim())
        .map_err(|e| anyhow!("bad trading_vault.curator_cap_id {}: {e}", p.cfg.curator_cap_id))?;
    let mut refs = VaultRefs {
        package: p.adapter_package,
        vault_id,
        curator_cap,
        registry: p.integration_registry,
        allowlist: p.pool_allowlist,
        custody_id: ObjectID::ZERO, // resolved below
    };

    // Resolve the custody: config pin, or create it once and log the id for
    // the operator to persist.
    refs.custody_id = if p.cfg.custody_id.trim().is_empty() {
        match init_custody(&wrap.client, &wrap.signer, &refs, p.db_cfg.gas_budget).await {
            Ok(id) => {
                tracing::info!(
                    custody_id = %id,
                    "DeepBook custody created — persist it as [trading_vault].custody_id"
                );
                id
            }
            Err(e) => {
                tracing::error!(
                    alert_id = ALERT_ID,
                    error = %format!("{e:#}"),
                    "init_custody tx failed; vault quoter cannot start"
                );
                return Err(e);
            }
        }
    } else {
        ObjectID::from_hex_literal(p.cfg.custody_id.trim())
            .map_err(|e| anyhow!("bad trading_vault.custody_id {}: {e}", p.cfg.custody_id))?
    };

    // pool_id → (base, quote) types, for gone-pool and shutdown cancels.
    let mut quoted_pools: HashMap<String, (String, String)> = HashMap::new();

    let mut ticker =
        tokio::time::interval(Duration::from_secs(p.db_cfg.quote_interval_secs.max(5)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Err(e) = cycle(&p, &wrap, &api, &refs, &mut quoted_pools).await {
                    tracing::warn!(error = %format!("{e:#}"), "vault-deepbook quote cycle failed; retrying next tick");
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!(pools = quoted_pools.len(), "shutdown: cancelling vault deepbook quotes");
                for (pool, (base, quote)) in &quoted_pools {
                    if let Ok(pool_id) = ObjectID::from_hex_literal(pool) {
                        if let Err(e) = cancel_all_on_pool(
                            &wrap.client, &wrap.signer, &refs,
                            pool_id, base, quote, p.db_cfg.gas_budget,
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
    p: &VaultQuoterParams,
    wrap: &SuiClientWrapper,
    api: &ApiServiceClient,
    refs: &VaultRefs,
    quoted_pools: &mut HashMap<String, (String, String)>,
) -> Result<()> {
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
            if let Ok(pool_id) = ObjectID::from_hex_literal(&pool) {
                tracing::info!(pool = %pool, "bucket left tradeable set; cancelling vault quotes");
                if let Err(e) = cancel_all_on_pool(
                    &wrap.client, &wrap.signer, refs,
                    pool_id, &base, &quote, p.db_cfg.gas_budget,
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
            market_spot(
                &p.price_cache,
                &p.markets[*mi],
                p.settlement_feed,
                p.settlement_decimals,
                p.staleness,
            )
        });
    }

    let now_expire = now + p.db_cfg.order_lifetime_secs.saturating_mul(1_000);
    let unix_minute = now / 60_000;

    let mut refreshes: Vec<VaultPoolRefresh> = Vec::new();
    // (pool_key, Some((call, settlement)) = placed; None = cancel-only)
    let mut book: Vec<(String, Option<(String, String)>)> = Vec::new();

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
        let cutoff_ms = p.db_cfg.expiry_cutoff_secs.saturating_mul(1_000);
        if b.expiry_ms.saturating_sub(now) < cutoff_ms {
            if quoted_pools.contains_key(&pool_key) {
                refreshes.push(VaultPoolRefresh {
                    pool_id,
                    base_coin_type: b.call_coin_type.clone(),
                    quote_coin_type: b.settlement_coin_type.clone(),
                    pool_index: refreshes.len(),
                    plan: QuotePlan { bid: None, ask: None, expire_timestamp_ms: now_expire },
                });
                book.push((pool_key, None));
            }
            continue;
        }

        let Some(BucketQuote { ask_raw, bid_raw, mid_raw: _, lot, min_size }) =
            price_bucket_quote(
                &p.pricing,
                &p.markets[*mi],
                b,
                p.db_cfg.quote_size,
                p.settlement_decimals,
                *spot_scaled,
                *sigma,
                now,
            )
        else {
            continue;
        };

        // Fixed per-side size (no inventory model: the custody's funding is
        // the curator's concern), rounded down to the pool's lot.
        let qty = (p.db_cfg.quote_size / lot) * lot;
        let plan = QuotePlan {
            bid: (qty >= min_size).then_some(QuoteSide { price_raw: bid_raw, quantity: qty }),
            ask: (qty >= min_size).then_some(QuoteSide { price_raw: ask_raw, quantity: qty }),
            expire_timestamp_ms: now_expire,
        };
        if plan.bid.is_none() && plan.ask.is_none() {
            if quoted_pools.contains_key(&pool_key) {
                refreshes.push(VaultPoolRefresh {
                    pool_id,
                    base_coin_type: b.call_coin_type.clone(),
                    quote_coin_type: b.settlement_coin_type.clone(),
                    pool_index: refreshes.len(),
                    plan,
                });
                book.push((pool_key, None));
            }
            continue;
        }

        refreshes.push(VaultPoolRefresh {
            pool_id,
            base_coin_type: b.call_coin_type.clone(),
            quote_coin_type: b.settlement_coin_type.clone(),
            pool_index: refreshes.len(),
            plan,
        });
        book.push((
            pool_key,
            Some((b.call_coin_type.clone(), b.settlement_coin_type.clone())),
        ));
    }

    if refreshes.is_empty() {
        return Ok(());
    }
    match refresh_pools_batched(
        &wrap.client,
        &wrap.signer,
        refs,
        &refreshes,
        unix_minute,
        p.db_cfg.max_pools_per_tx,
        p.db_cfg.gas_budget,
    )
    .await
    {
        Ok(resps) => {
            for (pool_key, upd) in book {
                match upd {
                    Some(pair) => {
                        quoted_pools.insert(pool_key, pair);
                    }
                    None => {
                        quoted_pools.remove(&pool_key);
                    }
                }
            }
            tracing::info!(
                pools = refreshes.len(),
                txs = resps.len(),
                "vault deepbook quotes refreshed (batched)"
            );
        }
        Err(e) => {
            tracing::error!(
                alert_id = ALERT_ID,
                error = %format!("{e:#}"),
                pools = refreshes.len(),
                "batched vault-deepbook refresh tx failed; leaving books as-is"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_order_id_packs_minute_pool_and_side() {
        // 2026-07-17T00:00Z ≈ minute 29_719_680.
        let minute = 29_719_680u64;
        let bid = client_order_id(minute, 3, false);
        let ask = client_order_id(minute, 3, true);
        assert_eq!(bid >> 16, minute);
        assert_eq!((bid >> 8) & 0xff, 3);
        assert_eq!(bid & 0xff, 0);
        assert_eq!(ask & 0xff, 1);
        // Same refresh's two sides share everything but the side byte.
        assert_eq!(bid | 1, ask);
    }

    #[test]
    fn client_order_id_masks_pool_index_to_a_byte() {
        // A pathological >255 pool index can't bleed into the minute bits.
        let a = client_order_id(1, 0x1_02, false);
        let b = client_order_id(1, 0x02, false);
        assert_eq!(a, b);
    }
}
