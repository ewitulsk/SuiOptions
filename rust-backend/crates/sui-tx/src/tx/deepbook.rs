//! DeepBook v3 PTB helpers for the mm-bot's quoting loop (SO-158).
//!
//! Everything here drives the **upgraded** DeepBook package (calls execute
//! there); only events resolve to the original publish, which is why
//! [`find_balance_manager`] queries by the original package id.
//!
//! The bot's BalanceManager is created once via
//! `new → register_balance_manager → public_share_object` — registering emits
//! `BalanceManagerEvent { balance_manager_id, owner }`, the only on-chain
//! breadcrumb that lets a restarted bot rediscover its BM without local state
//! (plain `new` emits nothing; verified in
//! `tools/deepbook-pool-test/DEEPBOOK-FINDINGS.md` §D).

use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use shared_crypto::intent::Intent;
use sui_json_rpc_types::{
    EventFilter, ObjectChange, SuiTransactionBlockEffectsAPI, SuiTransactionBlockResponse,
    SuiTransactionBlockResponseOptions,
};
use sui_sdk::SuiClient;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{
    Argument, ObjectArg, Transaction, TransactionData, TransactionKind,
};
use sui_types::transaction_driver_types::ExecuteTransactionRequestType;
use sui_types::SUI_CLOCK_OBJECT_ID;

use tracing::{debug, info};

use crate::sui_client::Signer;
use crate::tx::shared_object_arg;

/// DeepBook order-type / self-matching constants (deployed v3 values; an
/// unexpected drift aborts at dry-run, never on-chain).
pub const ORDER_TYPE_POST_ONLY: u8 = 3;
pub const SELF_MATCHING_CANCEL_TAKER: u8 = 1;

/// The DeepBook deployment the helpers talk to.
#[derive(Debug, Clone, Copy)]
pub struct DeepBookHandles {
    /// Upgraded package id — Move calls target this.
    pub package: ObjectID,
    /// Original publish id — event types resolve here.
    pub original_package: ObjectID,
    /// Shared `Registry` object.
    pub registry: ObjectID,
}

/// One side of a quote refresh: limit price in DeepBook raw price units
/// (quote-atomic per base-atomic × 10^9, rounded to tick) and quantity in
/// base atomic units (rounded to lot).
#[derive(Debug, Clone, Copy)]
pub struct QuoteSide {
    pub price_raw: u64,
    pub quantity: u64,
}

/// What [`refresh_pool_quotes`] should leave resting on the book.
#[derive(Debug, Clone, Copy, Default)]
pub struct QuotePlan {
    pub bid: Option<QuoteSide>,
    pub ask: Option<QuoteSide>,
    /// On-chain order expiry (unix ms) — stale quotes die even if the bot
    /// never comes back to cancel them.
    pub expire_timestamp_ms: u64,
}

/// Find the signer's registered BalanceManager by scanning
/// `BalanceManagerEvent`s (emitted by `register_balance_manager`, which our
/// creation flow always calls). Returns the newest match.
pub async fn find_balance_manager(
    client: &SuiClient,
    handles: &DeepBookHandles,
    owner: sui_types::base_types::SuiAddress,
) -> Result<Option<ObjectID>> {
    let event_type = format!(
        "{}::balance_manager::BalanceManagerEvent",
        handles.original_package
    );
    let filter = EventFilter::MoveEventType(
        event_type
            .parse()
            .with_context(|| format!("parsing event type {event_type}"))?,
    );
    let page = client
        .event_api()
        .query_events(filter, None, Some(50), true /* descending */)
        .await
        .context("querying BalanceManagerEvent")?;
    let owner_hex = owner.to_string();
    for ev in page.data {
        let json = &ev.parsed_json;
        if json.get("owner").and_then(|v| v.as_str()) == Some(owner_hex.as_str()) {
            if let Some(id) = json.get("balance_manager_id").and_then(|v| v.as_str()) {
                let id = ObjectID::from_hex_literal(id)
                    .with_context(|| format!("parsing balance_manager_id {id}"))?;
                return Ok(Some(id));
            }
        }
    }
    Ok(None)
}

/// Create + register + share a BalanceManager owned by the signer. Returns
/// the shared object's id. One-time per bot; rediscovery afterwards goes
/// through [`find_balance_manager`].
pub async fn create_balance_manager(
    client: &SuiClient,
    signer: &Signer,
    handles: &DeepBookHandles,
    gas_budget: u64,
) -> Result<ObjectID> {
    info!(owner = %signer.address, "creating DeepBook BalanceManager");
    let mut pt = ProgrammableTransactionBuilder::new();

    let bm = pt.programmable_move_call(
        handles.package,
        Identifier::new("balance_manager").unwrap(),
        Identifier::new("new").unwrap(),
        vec![],
        vec![],
    );
    // Registering is what emits BalanceManagerEvent{id, owner} — the durable
    // discovery record.
    let registry = pt.obj(shared_object_arg(client, handles.registry, true).await?)?;
    pt.programmable_move_call(
        handles.package,
        Identifier::new("balance_manager").unwrap(),
        Identifier::new("register_balance_manager").unwrap(),
        vec![],
        vec![bm, registry],
    );
    // BalanceManager has key+store but no module `share` fn — the PTB shares it.
    let bm_type = TypeTag::from_str(&format!(
        "{}::balance_manager::BalanceManager",
        handles.original_package
    ))
    .context("building BalanceManager type tag")?;
    pt.programmable_move_call(
        ObjectID::from_hex_literal("0x2").unwrap(),
        Identifier::new("transfer").unwrap(),
        Identifier::new("public_share_object").unwrap(),
        vec![bm_type],
        vec![bm],
    );

    let resp = submit(client, signer, pt, gas_budget).await?;
    let bm_suffix = "::balance_manager::BalanceManager";
    let created = resp
        .object_changes
        .unwrap_or_default()
        .into_iter()
        .find_map(|c| match c {
            sui_json_rpc_types::ObjectChange::Created {
                object_id,
                object_type,
                ..
            } if object_type.to_string().ends_with(bm_suffix) => Some(object_id),
            _ => None,
        })
        .ok_or_else(|| anyhow!("create tx succeeded but no BalanceManager in object changes"))?;
    info!(balance_manager = %created, "BalanceManager created + registered + shared");
    Ok(created)
}

/// DeepBook pool sizing derived from the pair's decimals. Kept in lockstep
/// with the frontend's `deriveVenueParams` (SO-154) and the mm-bot quoter so
/// every actor rounds prices/sizes onto the same grid — a mismatch makes an
/// order's dry run fail (price off tick). `tick` is a $0.01 price step
/// (scaled), `lot` an order-size step in base atomic units (10^3 floor),
/// `min` ten lots. Verified against live fills (DEEPBOOK-FINDINGS.md §C).
pub fn derived_pool_params(base_decimals: u8, quote_decimals: u8) -> (u64, u64, u64) {
    let price_exp = 9i32 - base_decimals as i32 + quote_decimals as i32;
    let tick = 10u64.pow((price_exp - 2).max(0) as u32);
    let lot = 10u64.pow((base_decimals as i32 - 5).max(3) as u32);
    let min = 10 * lot;
    (tick, lot, min)
}

/// Create a DeepBook permissionless pool for `base`/`quote` (base = the
/// bucket's Call coin, quote = the settlement asset). Gathers the fixed DEEP
/// creation fee from the signer's wallet, dry-run-gates the submit, and
/// returns the new shared Pool's id. Note the contract sends the fee to the
/// registry's `treasury_address`; on our self-owned deployment that is the
/// deployer, so the fee recycles (net-zero beyond gas). Dry-run reverts with
/// `EPoolAlreadyExists` if a pool for this pair already exists.
#[allow(clippy::too_many_arguments)]
pub async fn create_pool(
    client: &SuiClient,
    signer: &Signer,
    handles: &DeepBookHandles,
    deep_coin_type: &str,
    pool_creation_fee: u64,
    base_coin_type: &str,
    quote_coin_type: &str,
    base_decimals: u8,
    quote_decimals: u8,
    gas_budget: u64,
) -> Result<ObjectID> {
    let (tick, lot, min) = derived_pool_params(base_decimals, quote_decimals);
    let base_tag = TypeTag::from_str(base_coin_type)
        .with_context(|| format!("parsing base type {base_coin_type}"))?;
    let quote_tag = TypeTag::from_str(quote_coin_type)
        .with_context(|| format!("parsing quote type {quote_coin_type}"))?;

    let mut pt = ProgrammableTransactionBuilder::new();
    let registry = pt.obj(shared_object_arg(client, handles.registry, true).await?)?;
    let tick_arg = pt.pure(&tick)?;
    let lot_arg = pt.pure(&lot)?;
    let min_arg = pt.pure(&min)?;
    let fee_coin =
        gather_exact_coin(client, signer, &mut pt, deep_coin_type, pool_creation_fee).await?;
    pt.programmable_move_call(
        handles.package,
        Identifier::new("pool").unwrap(),
        Identifier::new("create_permissionless_pool").unwrap(),
        vec![base_tag, quote_tag],
        vec![registry, tick_arg, lot_arg, min_arg, fee_coin],
    );

    info!(base = %base_coin_type, quote = %quote_coin_type, tick, lot, min, "creating DeepBook pool");
    let resp = submit(client, signer, pt, gas_budget).await?;
    let pool = pool_id_from_changes(resp.object_changes.as_deref().unwrap_or(&[]))
        .ok_or_else(|| anyhow!("create_permissionless_pool succeeded but no Pool in object changes"))?;
    info!(pool = %pool, digest = %resp.digest, "DeepBook pool created");
    Ok(pool)
}

/// Pull the created `pool::Pool<_, _>` object id out of a tx's ObjectChanges.
fn pool_id_from_changes(changes: &[ObjectChange]) -> Option<ObjectID> {
    changes.iter().find_map(|c| match c {
        ObjectChange::Created {
            object_id,
            object_type,
            ..
        } if object_type.module.as_str() == "pool" && object_type.name.as_str() == "Pool" => {
            Some(*object_id)
        }
        _ => None,
    })
}

/// Read `balance_manager::balance<T>(&BM)` via dev-inspect (no gas, no
/// signature). Returns 0 for an asset the BM has never held.
pub async fn bm_balance(
    client: &SuiClient,
    sender: sui_types::base_types::SuiAddress,
    handles: &DeepBookHandles,
    bm_id: ObjectID,
    coin_type: &str,
) -> Result<u64> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let bm = pt.obj(shared_object_arg(client, bm_id, false).await?)?;
    let tag =
        TypeTag::from_str(coin_type).with_context(|| format!("parsing coin type {coin_type}"))?;
    pt.programmable_move_call(
        handles.package,
        Identifier::new("balance_manager").unwrap(),
        Identifier::new("balance").unwrap(),
        vec![tag],
        vec![bm],
    );
    let res = client
        .read_api()
        .dev_inspect_transaction_block(
            sender,
            TransactionKind::ProgrammableTransaction(pt.finish()),
            None,
            None,
            None,
        )
        .await
        .context("dev-inspecting balance_manager::balance")?;
    if let Some(err) = res.error {
        bail!("balance dev-inspect failed: {err}");
    }
    let results = res.results.unwrap_or_default();
    let (bytes, _) = results
        .last()
        .and_then(|r| r.return_values.first())
        .ok_or_else(|| anyhow!("balance dev-inspect returned no values"))?;
    bcs::from_bytes::<u64>(bytes).context("decoding balance u64")
}

/// Best bid/ask of one pool, in DeepBook raw price units. `None` on an
/// empty side — expected for fresh or one-sided books.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TopOfBook {
    pub best_bid_raw: Option<u64>,
    pub best_ask_raw: Option<u64>,
}

/// Read the top of one pool's book via dev-inspect (no gas, no signature).
/// Uses `pool::get_level2_ticks_from_mid(pool, 1, clock)` rather than
/// `pool::mid_price`, which aborts when either side is empty.
pub async fn top_of_book(
    client: &SuiClient,
    sender: sui_types::base_types::SuiAddress,
    deepbook_package: ObjectID,
    pool_id: ObjectID,
    base_coin_type: &str,
    quote_coin_type: &str,
) -> Result<TopOfBook> {
    let base_tag = TypeTag::from_str(base_coin_type)
        .with_context(|| format!("parsing base type {base_coin_type}"))?;
    let quote_tag = TypeTag::from_str(quote_coin_type)
        .with_context(|| format!("parsing quote type {quote_coin_type}"))?;

    let mut pt = ProgrammableTransactionBuilder::new();
    let pool = pt.obj(shared_object_arg(client, pool_id, false).await?)?;
    let ticks = pt.pure(&1u64)?;
    let clock = pt.obj(shared_object_arg(client, SUI_CLOCK_OBJECT_ID, false).await?)?;
    pt.programmable_move_call(
        deepbook_package,
        Identifier::new("pool").unwrap(),
        Identifier::new("get_level2_ticks_from_mid").unwrap(),
        vec![base_tag, quote_tag],
        vec![pool, ticks, clock],
    );
    let res = client
        .read_api()
        .dev_inspect_transaction_block(
            sender,
            TransactionKind::ProgrammableTransaction(pt.finish()),
            None,
            None,
            None,
        )
        .await
        .context("dev-inspecting pool::get_level2_ticks_from_mid")?;
    if let Some(err) = res.error {
        bail!("level2 dev-inspect failed: {err}");
    }
    // Returns four vectors: bid prices, bid quantities, ask prices, ask
    // quantities — best-first, so element 0 is the top of each side.
    let results = res.results.unwrap_or_default();
    let values = &results
        .last()
        .ok_or_else(|| anyhow!("level2 dev-inspect returned no results"))?
        .return_values;
    if values.len() < 4 {
        bail!("level2 dev-inspect returned {} values, expected 4", values.len());
    }
    let first_of = |i: usize| -> Result<Option<u64>> {
        let prices: Vec<u64> =
            bcs::from_bytes(&values[i].0).context("decoding level2 price vector")?;
        Ok(prices.first().copied())
    };
    Ok(TopOfBook {
        best_bid_raw: first_of(0)?,
        best_ask_raw: first_of(2)?,
    })
}

/// Atomically refresh the bot's resting quotes on one pool:
/// deposits (wallet → BM) → owner proof → settle → cancel-all → place bid/ask.
/// Every submit is dry-run gated; a refused refresh leaves the previous
/// orders standing (they self-expire via `expire_timestamp_ms`).
#[allow(clippy::too_many_arguments)]
pub async fn refresh_pool_quotes(
    client: &SuiClient,
    signer: &Signer,
    handles: &DeepBookHandles,
    pool_id: ObjectID,
    base_coin_type: &str,
    quote_coin_type: &str,
    bm_id: ObjectID,
    // `(coin_type, amount)` to move from the wallet into the BM first.
    deposits: &[(String, u64)],
    plan: QuotePlan,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    let base_tag = TypeTag::from_str(base_coin_type)
        .with_context(|| format!("parsing base type {base_coin_type}"))?;
    let quote_tag = TypeTag::from_str(quote_coin_type)
        .with_context(|| format!("parsing quote type {quote_coin_type}"))?;

    let mut pt = ProgrammableTransactionBuilder::new();
    let bm = pt.obj(shared_object_arg(client, bm_id, true).await?)?;

    // Wallet → BM deposits. Coins are gathered per type, merged, split to the
    // exact amount, deposited.
    for (coin_type, amount) in deposits {
        if *amount == 0 {
            continue;
        }
        let coin_arg = gather_exact_coin(client, signer, &mut pt, coin_type, *amount).await?;
        let tag = TypeTag::from_str(coin_type)
            .with_context(|| format!("parsing deposit type {coin_type}"))?;
        pt.programmable_move_call(
            handles.package,
            Identifier::new("balance_manager").unwrap(),
            Identifier::new("deposit").unwrap(),
            vec![tag],
            vec![bm, coin_arg],
        );
    }

    let proof = pt.programmable_move_call(
        handles.package,
        Identifier::new("balance_manager").unwrap(),
        Identifier::new("generate_proof_as_owner").unwrap(),
        vec![],
        vec![bm],
    );

    let pool = pt.obj(shared_object_arg(client, pool_id, true).await?)?;
    let clock = pt.obj(shared_object_arg(client, SUI_CLOCK_OBJECT_ID, false).await?)?;

    // Fills since the last refresh sit in the pool as settled funds — pull
    // them back into the BM so the new orders can spend them.
    pt.programmable_move_call(
        handles.package,
        Identifier::new("pool").unwrap(),
        Identifier::new("withdraw_settled_amounts").unwrap(),
        vec![base_tag.clone(), quote_tag.clone()],
        vec![pool, bm, proof],
    );
    pt.programmable_move_call(
        handles.package,
        Identifier::new("pool").unwrap(),
        Identifier::new("cancel_all_orders").unwrap(),
        vec![base_tag.clone(), quote_tag.clone()],
        vec![pool, bm, proof, clock],
    );

    let place = |pt: &mut ProgrammableTransactionBuilder,
                     side: QuoteSide,
                     is_bid: bool|
     -> Result<()> {
        let client_order_id = pt.pure(&plan.expire_timestamp_ms)?;
        let order_type = pt.pure(&ORDER_TYPE_POST_ONLY)?;
        let self_matching = pt.pure(&SELF_MATCHING_CANCEL_TAKER)?;
        let price = pt.pure(&side.price_raw)?;
        let qty = pt.pure(&side.quantity)?;
        let is_bid_arg = pt.pure(&is_bid)?;
        let pay_with_deep = pt.pure(&false)?;
        let expire = pt.pure(&plan.expire_timestamp_ms)?;
        pt.programmable_move_call(
            handles.package,
            Identifier::new("pool").unwrap(),
            Identifier::new("place_limit_order").unwrap(),
            vec![base_tag.clone(), quote_tag.clone()],
            vec![
                pool,
                bm,
                proof,
                client_order_id,
                order_type,
                self_matching,
                price,
                qty,
                is_bid_arg,
                pay_with_deep,
                expire,
                clock,
            ],
        );
        Ok(())
    };
    if let Some(bid) = plan.bid {
        place(&mut pt, bid, true)?;
    }
    if let Some(ask) = plan.ask {
        place(&mut pt, ask, false)?;
    }

    submit(client, signer, pt, gas_budget).await
}

/// One pool's desired resting quote, for a batched multi-pool refresh.
#[derive(Debug, Clone)]
pub struct PoolRefresh {
    pub pool_id: ObjectID,
    /// Pool base = the bucket's call coin.
    pub base_coin_type: String,
    /// Pool quote = the settlement asset.
    pub quote_coin_type: String,
    pub plan: QuotePlan,
}

/// Refresh many pools' quotes in as few transactions as possible (SO-173).
///
/// One BalanceManager backs every pool, so `deposits` (already aggregated per
/// coin type across the whole batch — you can't gather the same coins twice in
/// one PTB) fund it once in the first tx, and a single trade proof is generated
/// per tx and reused across every pool in it. Pools are packed
/// `max_pools_per_tx` at a time to stay under PTB limits; later chunks spend the
/// balance the first chunk's deposits left in the shared BM. Each chunk runs
/// `withdraw_settled → cancel_all → place bid/ask` per pool — a pool with an
/// empty plan is therefore a cancel-only entry. Every submit is dry-run gated;
/// returns one response per chunk.
pub async fn refresh_pools_batched(
    client: &SuiClient,
    signer: &Signer,
    handles: &DeepBookHandles,
    bm_id: ObjectID,
    deposits: &[(String, u64)],
    refreshes: &[PoolRefresh],
    max_pools_per_tx: usize,
    gas_budget: u64,
) -> Result<Vec<SuiTransactionBlockResponse>> {
    if refreshes.is_empty() {
        return Ok(Vec::new());
    }
    let chunk = max_pools_per_tx.max(1);
    let mut responses = Vec::with_capacity(refreshes.len() / chunk + 1);

    for (ci, group) in refreshes.chunks(chunk).enumerate() {
        let mut pt = ProgrammableTransactionBuilder::new();
        let bm = pt.obj(shared_object_arg(client, bm_id, true).await?)?;

        // Deposits ride in the first tx only; the BM is shared, so later chunks
        // spend the funded balance.
        if ci == 0 {
            for (coin_type, amount) in deposits {
                if *amount == 0 {
                    continue;
                }
                let coin_arg =
                    gather_exact_coin(client, signer, &mut pt, coin_type, *amount).await?;
                let tag = TypeTag::from_str(coin_type)
                    .with_context(|| format!("parsing deposit type {coin_type}"))?;
                pt.programmable_move_call(
                    handles.package,
                    Identifier::new("balance_manager").unwrap(),
                    Identifier::new("deposit").unwrap(),
                    vec![tag],
                    vec![bm, coin_arg],
                );
            }
        }

        // One proof per tx, reused across every pool (it's bound to the BM, not
        // a pool, and place/cancel take it by reference).
        let proof = pt.programmable_move_call(
            handles.package,
            Identifier::new("balance_manager").unwrap(),
            Identifier::new("generate_proof_as_owner").unwrap(),
            vec![],
            vec![bm],
        );
        let clock = pt.obj(shared_object_arg(client, SUI_CLOCK_OBJECT_ID, false).await?)?;

        for r in group {
            let base_tag = TypeTag::from_str(&r.base_coin_type)
                .with_context(|| format!("parsing base type {}", r.base_coin_type))?;
            let quote_tag = TypeTag::from_str(&r.quote_coin_type)
                .with_context(|| format!("parsing quote type {}", r.quote_coin_type))?;
            let pool = pt.obj(shared_object_arg(client, r.pool_id, true).await?)?;

            pt.programmable_move_call(
                handles.package,
                Identifier::new("pool").unwrap(),
                Identifier::new("withdraw_settled_amounts").unwrap(),
                vec![base_tag.clone(), quote_tag.clone()],
                vec![pool, bm, proof],
            );
            pt.programmable_move_call(
                handles.package,
                Identifier::new("pool").unwrap(),
                Identifier::new("cancel_all_orders").unwrap(),
                vec![base_tag.clone(), quote_tag.clone()],
                vec![pool, bm, proof, clock],
            );

            let place = |pt: &mut ProgrammableTransactionBuilder,
                         side: QuoteSide,
                         is_bid: bool|
             -> Result<()> {
                let client_order_id = pt.pure(&r.plan.expire_timestamp_ms)?;
                let order_type = pt.pure(&ORDER_TYPE_POST_ONLY)?;
                let self_matching = pt.pure(&SELF_MATCHING_CANCEL_TAKER)?;
                let price = pt.pure(&side.price_raw)?;
                let qty = pt.pure(&side.quantity)?;
                let is_bid_arg = pt.pure(&is_bid)?;
                let pay_with_deep = pt.pure(&false)?;
                let expire = pt.pure(&r.plan.expire_timestamp_ms)?;
                pt.programmable_move_call(
                    handles.package,
                    Identifier::new("pool").unwrap(),
                    Identifier::new("place_limit_order").unwrap(),
                    vec![base_tag.clone(), quote_tag.clone()],
                    vec![
                        pool,
                        bm,
                        proof,
                        client_order_id,
                        order_type,
                        self_matching,
                        price,
                        qty,
                        is_bid_arg,
                        pay_with_deep,
                        expire,
                        clock,
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

        responses.push(submit(client, signer, pt, gas_budget).await?);
    }
    Ok(responses)
}

/// Cancel everything the BM has resting on `pool` (shutdown / pool-exit path).
pub async fn cancel_all_on_pool(
    client: &SuiClient,
    signer: &Signer,
    handles: &DeepBookHandles,
    pool_id: ObjectID,
    base_coin_type: &str,
    quote_coin_type: &str,
    bm_id: ObjectID,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    let base_tag = TypeTag::from_str(base_coin_type)?;
    let quote_tag = TypeTag::from_str(quote_coin_type)?;
    let mut pt = ProgrammableTransactionBuilder::new();
    let bm = pt.obj(shared_object_arg(client, bm_id, true).await?)?;
    let proof = pt.programmable_move_call(
        handles.package,
        Identifier::new("balance_manager").unwrap(),
        Identifier::new("generate_proof_as_owner").unwrap(),
        vec![],
        vec![bm],
    );
    let pool = pt.obj(shared_object_arg(client, pool_id, true).await?)?;
    let clock = pt.obj(shared_object_arg(client, SUI_CLOCK_OBJECT_ID, false).await?)?;
    pt.programmable_move_call(
        handles.package,
        Identifier::new("pool").unwrap(),
        Identifier::new("cancel_all_orders").unwrap(),
        vec![base_tag, quote_tag],
        vec![pool, bm, proof, clock],
    );
    submit(client, signer, pt, gas_budget).await
}

/// Gather wallet coins of `coin_type`, merge into one, split off `amount`.
/// Returns the exact-amount coin argument.
async fn gather_exact_coin(
    client: &SuiClient,
    signer: &Signer,
    pt: &mut ProgrammableTransactionBuilder,
    coin_type: &str,
    amount: u64,
) -> Result<Argument> {
    let coins = client
        .coin_read_api()
        .get_coins(signer.address, Some(coin_type.to_string()), None, Some(50))
        .await
        .with_context(|| format!("listing {coin_type} coins"))?
        .data;
    let total: u128 = coins.iter().map(|c| c.balance as u128).sum();
    if total < amount as u128 {
        bail!("wallet holds {total} of {coin_type}, need {amount}");
    }
    let mut refs = coins.into_iter().map(|c| c.object_ref());
    let first = refs.next().ok_or_else(|| anyhow!("no {coin_type} coins"))?;
    let primary = pt.obj(ObjectArg::ImmOrOwnedObject(first))?;
    let rest: Vec<Argument> = refs
        .map(|r| pt.obj(ObjectArg::ImmOrOwnedObject(r)))
        .collect::<Result<_, _>>()?;
    if !rest.is_empty() {
        pt.command(sui_types::transaction::Command::MergeCoins(primary, rest));
    }
    let amt = pt.pure(&amount)?;
    let split = pt.command(sui_types::transaction::Command::SplitCoins(
        primary,
        vec![amt],
    ));
    Ok(match split {
        Argument::Result(i) => Argument::NestedResult(i, 0),
        other => other,
    })
}

/// Dry-run gate + sign + execute. Mirrors `test_tokens::submit`.
async fn submit(
    client: &SuiClient,
    signer: &Signer,
    pt: ProgrammableTransactionBuilder,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    let programmable = pt.finish();
    let gas_coin = client
        .coin_read_api()
        .get_coins(signer.address, None, None, Some(5))
        .await
        .context("listing gas coins")?
        .data
        .into_iter()
        .max_by_key(|c| c.balance)
        .ok_or_else(|| anyhow!("no SUI coins to pay gas for {}", signer.address))?;
    let gas_price = client
        .read_api()
        .get_reference_gas_price()
        .await
        .context("fetching reference gas price")?;
    let tx_data = TransactionData::new_programmable(
        signer.address,
        vec![gas_coin.object_ref()],
        programmable,
        gas_budget,
        gas_price,
    );

    // Dry-run first so a bad assumption (book moved, POST-only would cross,
    // wrong constant) costs nothing and is loudly attributable.
    let dry = client
        .read_api()
        .dry_run_transaction_block(tx_data.clone())
        .await
        .context("dry-running deepbook tx")?;
    if dry.effects.status().is_err() {
        bail!("deepbook tx dry-run reverted: {:?}", dry.effects.status());
    }

    let sig = Transaction::signature_from_signer(
        tx_data.clone(),
        Intent::sui_transaction(),
        &signer.keypair,
    );
    let tx = Transaction::from_data(tx_data, vec![sig]);
    let opts = SuiTransactionBlockResponseOptions::new()
        .with_effects()
        .with_object_changes();
    let resp = client
        .quorum_driver_api()
        .execute_transaction_block(
            tx,
            opts,
            Some(ExecuteTransactionRequestType::WaitForLocalExecution),
        )
        .await
        .context("submitting deepbook tx")?;
    let effects = resp.effects.as_ref().context("response missing effects")?;
    if effects.status().is_err() {
        bail!("deepbook tx reverted: {:?}", effects.status());
    }
    debug!(digest = %resp.digest, "deepbook tx succeeded");
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_params_match_frontend_grid() {
        // 8-dec base / 6-dec quote: price exponent 7 → tick 1e5 ($0.01),
        // lot 1e3, min 1e4 — the numbers `deriveVenueParams` produces and the
        // mm-bot quoter rounds against.
        assert_eq!(derived_pool_params(8, 6), (100_000, 1_000, 10_000));
        // 9-dec base (TWAL-style): exponent 6 → tick 1e4, lot 1e4, min 1e5.
        assert_eq!(derived_pool_params(9, 6), (10_000, 10_000, 100_000));
    }
}
