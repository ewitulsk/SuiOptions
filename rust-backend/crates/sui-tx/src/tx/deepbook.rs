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
    EventFilter, SuiTransactionBlockEffectsAPI, SuiTransactionBlockResponse,
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
