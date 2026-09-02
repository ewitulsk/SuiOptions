//! Put-exercise PTBs for the mm-bot desk (SO-443, doc 08 §4.4).
//!
//! A put is exercised by DELIVERING `amount` underlying and receiving
//! `floor(amount × strike)` settlement (`put_bucket::exercise`). Every
//! path here restores or repays the underlying it delivered and leaves
//! the residual profit in settlement at `recipient`, in ONE atomic PTB:
//!
//! 1. [`PutPath::VaultUnderlying`] — deliver own underlying, repurchase
//!    the delivered amount out of the payout, keep the residual.
//! 2. [`PutPath::BaseFlash`] — `borrow_flashloan_base(amount)` →
//!    exercise → buy the EXACT repayment amount from the payout →
//!    `return_flashloan_base` → residual.
//! 3. [`PutPath::QuoteFlash`] — `borrow_flashloan_quote(max_quote_in)`
//!    → buy the required underlying → exercise → repay settlement from
//!    the payout → residual.
//!
//! Bounds are STRUCTURAL: the repurchase spends at most `max_quote_in`
//! settlement and must return at least `amount` underlying
//! (`min_base_out`), flash repayment is an exact `SplitCoins` of the
//! borrowed amount, and the minimum profit is asserted on-chain by
//! splitting `min_profit` off the residual (an under-funded split
//! aborts). `max_quote_in + min_profit ≤ payout` is checked before a
//! single command is emitted. The builders are chain-free so their shape
//! is golden-tested; [`submit_put_exercise`] resolves object refs,
//! dev-inspects (status + gas bound) and only then signs.
//!
//! The vault-custody twin ([`build_vault_put_exercise`]) runs entirely
//! inside curator sessions: `vault_mm::exercise_put_coin` delivers vault
//! free underlying and `deepbook_adapter::taker_swap_quote_for_base`
//! repurchases it on an ALLOWLISTED pool — nothing leaves the vault.

use std::str::FromStr;

use anyhow::{bail, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{
    Argument, CallArg, Command, ProgrammableTransaction, TransactionData,
};
use sui_types::SUI_CLOCK_OBJECT_ID;

use crate::chain::{decode_return_value, ChainClient, ExecutedTransaction};
use crate::sui_client::Signer;
use crate::tx::deepbook::{gather_exact_coin, nested, submit_programmable, zero_coin};
use crate::tx::shared_object_arg;

// The route enum, the pool-liquidity shape and the lot/ladder math are
// strategy inputs the desk kernel plans on, so they live in `desk-core`
// (SO-450) and are re-exported here for the PTB builders and callers.
pub use desk_core::exits::put::{
    lot_round_up, quote_needed_for_base, PoolLiquidity, PutPath, FLOAT_SCALING,
};

/// Chain-free inputs for one wallet put-exercise PTB.
#[derive(Clone, Debug)]
pub struct PutPtbSpec<'a> {
    /// Upgraded DeepBook package (calls execute here).
    pub deepbook_package: ObjectID,
    /// options_core package (`put_bucket::exercise`).
    pub core_package: ObjectID,
    pub underlying_type: &'a str,
    pub settlement_type: &'a str,
    pub put_coin_type: &'a str,
    /// The deployment's DEEP token type (fee leg; zero coin passed).
    pub deep_coin_type: &'a str,
    /// Put units to exercise = underlying units delivered.
    pub amount: u64,
    /// `put_bucket::exercise_payout` — floor(amount × strike).
    pub payout: u64,
    /// Explicit MAX settlement spent repurchasing the underlying (and the
    /// quote-flash principal).
    pub max_quote_in: u64,
    /// Settlement that must remain after every repayment — asserted
    /// on-chain by a `SplitCoins` of the residual.
    pub min_profit: u64,
    /// Where the residual settlement and every residue land — the vault.
    pub recipient: SuiAddress,
}

/// PTB arguments the builder consumes (resolved by the caller so the
/// builder stays chain-free).
#[derive(Clone, Copy, Debug)]
pub struct PutPtbArgs {
    pub pool: Argument,
    pub bucket: Argument,
    pub clock: Argument,
    /// `Coin<Put>` of exactly `spec.amount`.
    pub puts: Argument,
    /// `Coin<Underlying>` of exactly `spec.amount` — required by
    /// [`PutPath::VaultUnderlying`], ignored by the flash paths.
    pub underlying: Option<Argument>,
}

fn pool_tags(spec: &PutPtbSpec<'_>) -> Result<Vec<TypeTag>> {
    Ok(vec![
        TypeTag::from_str(spec.underlying_type)?,
        TypeTag::from_str(spec.settlement_type)?,
    ])
}

fn bucket_tags(spec: &PutPtbSpec<'_>) -> Result<Vec<TypeTag>> {
    Ok(vec![
        TypeTag::from_str(spec.underlying_type)?,
        TypeTag::from_str(spec.settlement_type)?,
        TypeTag::from_str(spec.put_coin_type)?,
    ])
}

fn pool_call(
    pt: &mut ProgrammableTransactionBuilder,
    spec: &PutPtbSpec<'_>,
    function: &str,
    args: Vec<Argument>,
) -> Result<Argument> {
    Ok(pt.programmable_move_call(
        spec.deepbook_package,
        Identifier::new("pool").unwrap(),
        Identifier::new(function).unwrap(),
        pool_tags(spec)?,
        args,
    ))
}

/// `put_bucket::exercise(bucket, puts, delivery, clock)` → `Coin<S>`.
fn exercise_call(
    pt: &mut ProgrammableTransactionBuilder,
    spec: &PutPtbSpec<'_>,
    args: &PutPtbArgs,
    delivery: Argument,
) -> Result<Argument> {
    Ok(pt.programmable_move_call(
        spec.core_package,
        Identifier::new("put_bucket").unwrap(),
        Identifier::new("exercise").unwrap(),
        bucket_tags(spec)?,
        vec![args.bucket, args.puts, delivery, args.clock],
    ))
}

/// `swap_exact_quote_for_base(pool, quote_in, DEEP::zero, min_base_out =
/// amount, clock)` → `(base_out, quote_rem, deep_rem)`.
fn repurchase(
    pt: &mut ProgrammableTransactionBuilder,
    spec: &PutPtbSpec<'_>,
    args: &PutPtbArgs,
    quote_in: Argument,
) -> Result<(Argument, Argument, Argument)> {
    let deep_zero = zero_coin(pt, spec.deep_coin_type)?;
    let min_out = pt.pure(&spec.amount)?;
    let swap = pool_call(
        pt,
        spec,
        "swap_exact_quote_for_base",
        vec![args.pool, quote_in, deep_zero, min_out, args.clock],
    )?;
    Ok((nested(swap, 0), nested(swap, 1), nested(swap, 2)))
}

fn split(pt: &mut ProgrammableTransactionBuilder, coin: Argument, amount: u64) -> Result<Argument> {
    let amt = pt.pure(&amount)?;
    Ok(nested(pt.command(Command::SplitCoins(coin, vec![amt])), 0))
}

/// Check the spec's bounds before any command is emitted.
fn check_bounds(spec: &PutPtbSpec<'_>) -> Result<()> {
    if spec.amount == 0 {
        bail!("put exercise: zero amount");
    }
    if spec.max_quote_in == 0 {
        bail!("put exercise: zero max_quote_in (no repurchase budget)");
    }
    let needed = spec.max_quote_in.saturating_add(spec.min_profit);
    if needed > spec.payout {
        bail!(
            "put exercise profit bound: max_quote_in {} + min_profit {} exceeds payout {}",
            spec.max_quote_in,
            spec.min_profit,
            spec.payout
        );
    }
    Ok(())
}

/// Emit one wallet put-exercise route into `pt`. Every coin or hot potato
/// the route receives is consumed or transferred to `spec.recipient`
/// (checked by [`assert_nothing_stranded`] in tests).
pub fn build_put_exercise(
    pt: &mut ProgrammableTransactionBuilder,
    spec: &PutPtbSpec<'_>,
    args: &PutPtbArgs,
    path: PutPath,
) -> Result<()> {
    check_bounds(spec)?;
    let recipient = pt.pure(&spec.recipient)?;
    match path {
        PutPath::VaultUnderlying => {
            let underlying = args
                .underlying
                .context("vault-underlying path needs an underlying coin argument")?;
            // 1. Deliver own underlying, receive the strike payout.
            let payout = exercise_call(pt, spec, args, underlying)?;
            // 2. Repurchase the delivered amount out of ≤ max_quote_in.
            let spend = split(pt, payout, spec.max_quote_in)?;
            let (base_out, quote_rem, deep_rem) = repurchase(pt, spec, args, spend)?;
            // 3. Minimum-profit assertion: the residual must cover it.
            let profit = split(pt, payout, spec.min_profit)?;
            pt.command(Command::TransferObjects(
                vec![payout, profit, base_out, quote_rem, deep_rem],
                recipient,
            ));
        }
        PutPath::BaseFlash => {
            // 1. Borrow the delivery from the spot pool.
            let amt = pt.pure(&spec.amount)?;
            let borrow = pool_call(pt, spec, "borrow_flashloan_base", vec![args.pool, amt])?;
            let borrowed = nested(borrow, 0);
            let loan = nested(borrow, 1);
            // 2. Exercise with the borrowed underlying.
            let payout = exercise_call(pt, spec, args, borrowed)?;
            // 3. Buy back ≥ amount out of ≤ max_quote_in of the payout.
            let spend = split(pt, payout, spec.max_quote_in)?;
            let (base_out, quote_rem, deep_rem) = repurchase(pt, spec, args, spend)?;
            // 4. Exact repayment.
            let repay = split(pt, base_out, spec.amount)?;
            pool_call(pt, spec, "return_flashloan_base", vec![args.pool, repay, loan])?;
            // 5. Minimum-profit assertion + everything to the vault.
            let profit = split(pt, payout, spec.min_profit)?;
            pt.command(Command::TransferObjects(
                vec![payout, profit, base_out, quote_rem, deep_rem],
                recipient,
            ));
        }
        PutPath::QuoteFlash => {
            // 1. Borrow the repurchase budget in settlement.
            let amt = pt.pure(&spec.max_quote_in)?;
            let borrow = pool_call(pt, spec, "borrow_flashloan_quote", vec![args.pool, amt])?;
            let borrowed = nested(borrow, 0);
            let loan = nested(borrow, 1);
            // 2. Buy ≥ amount underlying with it.
            let (base_out, quote_rem, deep_rem) = repurchase(pt, spec, args, borrowed)?;
            // 3. Deliver exactly `amount`; the lot residue goes to the vault.
            let delivery = split(pt, base_out, spec.amount)?;
            let payout = exercise_call(pt, spec, args, delivery)?;
            // 4. Repay the exact principal from payout + unspent quote.
            pt.command(Command::MergeCoins(payout, vec![quote_rem]));
            let repay = split(pt, payout, spec.max_quote_in)?;
            pool_call(pt, spec, "return_flashloan_quote", vec![args.pool, repay, loan])?;
            // 5. Minimum-profit assertion + everything to the vault.
            let profit = split(pt, payout, spec.min_profit)?;
            pt.command(Command::TransferObjects(
                vec![payout, profit, base_out, deep_rem],
                recipient,
            ));
        }
    }
    Ok(())
}

// ── vault-custody path (curator session) ───────────────────────────────

/// Chain-free inputs for the vault-custody put exercise.
#[derive(Clone, Debug)]
pub struct VaultPutPtbSpec<'a> {
    pub trading_vault_package: ObjectID,
    pub deepbook_adapter_package: ObjectID,
    pub underlying_type: &'a str,
    pub settlement_type: &'a str,
    pub put_coin_type: &'a str,
    /// The VaultMm coin-custody position holding the put coins.
    pub coin_position_id: ObjectID,
    pub amount: u64,
    pub payout: u64,
    /// Max vault settlement spent repurchasing `amount` underlying.
    pub max_quote_in: u64,
    pub min_profit: u64,
}

/// `(vault, cap, reg)` + allowlist/pool/bucket/clock arguments.
#[derive(Clone, Copy, Debug)]
pub struct VaultPutPtbArgs {
    pub vault: Argument,
    pub cap: Argument,
    pub reg: Argument,
    pub allowlist: Argument,
    pub pool: Argument,
    pub bucket: Argument,
    pub clock: Argument,
}

/// `vault_mm::exercise_put_coin` (vault free underlying → strike payout
/// into free settlement) followed by
/// `deepbook_adapter::taker_swap_quote_for_base` (≤ `max_quote_in`
/// settlement → ≥ `amount` underlying) on an allowlisted pool. Profit is
/// structural: `max_quote_in + min_profit ≤ payout`.
pub fn build_vault_put_exercise(
    pt: &mut ProgrammableTransactionBuilder,
    spec: &VaultPutPtbSpec<'_>,
    args: &VaultPutPtbArgs,
) -> Result<()> {
    if spec.amount == 0 || spec.max_quote_in == 0 {
        bail!("vault put exercise: zero amount or repurchase budget");
    }
    if spec.max_quote_in.saturating_add(spec.min_profit) > spec.payout {
        bail!(
            "vault put exercise profit bound: max_quote_in {} + min_profit {} exceeds payout {}",
            spec.max_quote_in,
            spec.min_profit,
            spec.payout
        );
    }
    let coin_position_id = pt.pure(&spec.coin_position_id)?;
    let amount = pt.pure(&spec.amount)?;
    pt.programmable_move_call(
        spec.trading_vault_package,
        Identifier::new("vault_mm").unwrap(),
        Identifier::new("exercise_put_coin").unwrap(),
        vec![
            TypeTag::from_str(spec.underlying_type)?,
            TypeTag::from_str(spec.settlement_type)?,
            TypeTag::from_str(spec.put_coin_type)?,
        ],
        vec![args.vault, args.cap, args.reg, args.bucket, coin_position_id, amount, args.clock],
    );
    let quote_in = pt.pure(&spec.max_quote_in)?;
    let min_out = pt.pure(&spec.amount)?;
    pt.programmable_move_call(
        spec.deepbook_adapter_package,
        Identifier::new("deepbook_adapter").unwrap(),
        Identifier::new("taker_swap_quote_for_base").unwrap(),
        vec![
            TypeTag::from_str(spec.underlying_type)?,
            TypeTag::from_str(spec.settlement_type)?,
        ],
        vec![
            args.vault,
            args.cap,
            args.reg,
            args.allowlist,
            args.pool,
            quote_in,
            min_out,
            args.clock,
        ],
    );
    Ok(())
}

// ── pool reads (dev-inspect) ───────────────────────────────────────────

async fn inspect_pool_read(
    client: &ChainClient,
    sender: SuiAddress,
    deepbook_package: ObjectID,
    pool_id: ObjectID,
    tags: &[TypeTag],
    function: &str,
    extra: impl FnOnce(&mut ProgrammableTransactionBuilder) -> Result<Vec<Argument>>,
) -> Result<sui_rpc_api::client::SimulateTransactionResponse> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let pool = pt.obj(shared_object_arg(client, pool_id, false).await?)?;
    let mut args = vec![pool];
    args.extend(extra(&mut pt)?);
    pt.programmable_move_call(
        deepbook_package,
        Identifier::new("pool").unwrap(),
        Identifier::new(function).unwrap(),
        tags.to_vec(),
        args,
    );
    client
        .dev_inspect_ptb(sender, pt)
        .await
        .with_context(|| format!("dev-inspecting pool::{function}"))
}

/// Read flash capacity, lot/min size and the ask ladder (`ticks` levels)
/// of one pool — three gas-less dev-inspects.
pub async fn pool_liquidity(
    client: &ChainClient,
    sender: SuiAddress,
    deepbook_package: ObjectID,
    pool_id: ObjectID,
    base_coin_type: &str,
    quote_coin_type: &str,
    ticks: u64,
) -> Result<PoolLiquidity> {
    let tags = vec![
        TypeTag::from_str(base_coin_type)?,
        TypeTag::from_str(quote_coin_type)?,
    ];
    let bal = inspect_pool_read(
        client,
        sender,
        deepbook_package,
        pool_id,
        &tags,
        "vault_balances",
        |_| Ok(vec![]),
    )
    .await?;
    let params = inspect_pool_read(
        client,
        sender,
        deepbook_package,
        pool_id,
        &tags,
        "pool_book_params",
        |_| Ok(vec![]),
    )
    .await?;
    let l2 = inspect_pool_read(
        client,
        sender,
        deepbook_package,
        pool_id,
        &tags,
        "get_level2_ticks_from_mid",
        |pt| {
            let t = pt.pure(&ticks)?;
            let clock = pt.obj(sui_types::transaction::ObjectArg::SharedObject {
                id: SUI_CLOCK_OBJECT_ID,
                initial_shared_version: sui_types::SUI_CLOCK_OBJECT_SHARED_VERSION,
                mutability: sui_types::transaction::SharedObjectMutability::Immutable,
            })?;
            Ok(vec![t, clock])
        },
    )
    .await?;
    let ask_prices: Vec<u64> = decode_return_value(&l2, 2).context("decoding ask prices")?;
    let ask_qtys: Vec<u64> = decode_return_value(&l2, 3).context("decoding ask quantities")?;
    Ok(PoolLiquidity {
        base_balance: decode_return_value(&bal, 0).context("decoding base balance")?,
        quote_balance: decode_return_value(&bal, 1).context("decoding quote balance")?,
        lot_size: decode_return_value(&params, 1).context("decoding lot size")?,
        min_size: decode_return_value(&params, 2).context("decoding min size")?,
        asks: ask_prices.into_iter().zip(ask_qtys).collect(),
    })
}

// ── pre-simulate + submit ──────────────────────────────────────────────

/// Object ids and bounds for [`submit_put_exercise`].
#[derive(Clone, Copy, Debug)]
pub struct PutSubmitRefs {
    pub spot_pool: ObjectID,
    pub bucket: ObjectID,
    pub gas_budget: u64,
    /// Dev-inspect gas (computation + storage, MIST) above which the PTB
    /// is NOT submitted.
    pub max_gas_mist: u64,
}

/// Gather the wallet coins, build `path`, dev-inspect (status + gas
/// bound) and only then sign and submit. Any failure returns before a
/// signature exists; an on-chain abort reverts the whole PTB.
pub async fn submit_put_exercise(
    client: &ChainClient,
    signer: &Signer,
    spec: &PutPtbSpec<'_>,
    refs: &PutSubmitRefs,
    path: PutPath,
) -> Result<ExecutedTransaction> {
    check_bounds(spec)?;
    let mut pt = ProgrammableTransactionBuilder::new();
    let pool = pt.obj(shared_object_arg(client, refs.spot_pool, true).await?)?;
    let bucket = pt.obj(shared_object_arg(client, refs.bucket, true).await?)?;
    let clock = crate::tx::clock_arg(&mut pt)?;
    let puts = gather_exact_coin(client, signer, &mut pt, spec.put_coin_type, spec.amount).await?;
    let underlying = match path {
        PutPath::VaultUnderlying => Some(
            gather_exact_coin(client, signer, &mut pt, spec.underlying_type, spec.amount)
                .await
                .context("gathering own underlying for delivery")?,
        ),
        _ => None,
    };
    let args = PutPtbArgs { pool, bucket, clock, puts, underlying };
    build_put_exercise(&mut pt, spec, &args, path)?;
    let programmable = pt.finish();
    presimulate(client, signer.address, &programmable, refs).await?;
    submit_programmable(client, signer, programmable, refs.gas_budget).await
}

/// Dev-inspect `programmable` for `sender`: revert status or a gas cost
/// above `refs.max_gas_mist` is an error (nothing signed).
pub async fn presimulate(
    client: &ChainClient,
    sender: SuiAddress,
    programmable: &ProgrammableTransaction,
    refs: &PutSubmitRefs,
) -> Result<()> {
    let inspect_tx = TransactionData::new_programmable(
        sender,
        vec![],
        programmable.clone(),
        refs.gas_budget,
        client.reference_gas_price().await?,
    );
    let inspect = client
        .dev_inspect(&inspect_tx)
        .await
        .context("dev-inspecting put exercise")?;
    use sui_types::effects::TransactionEffectsAPI;
    let status = inspect.transaction.effects.status();
    if status.is_err() {
        bail!(
            "put exercise pre-simulation failed (slippage/capacity/repayment/profit bound): \
             {status:?}"
        );
    }
    let gas = gas_used_mist(&inspect.transaction.effects);
    if gas > refs.max_gas_mist {
        bail!("put exercise gas bound: simulated {gas} MIST exceeds max {}", refs.max_gas_mist);
    }
    Ok(())
}

/// Computation + storage cost of simulated effects (MIST).
pub fn gas_used_mist(effects: &sui_types::effects::TransactionEffects) -> u64 {
    use sui_types::effects::TransactionEffectsAPI;
    let s = effects.gas_cost_summary();
    s.computation_cost.saturating_add(s.storage_cost)
}

// ── PTB-shape checker (tests) ──────────────────────────────────────────

/// Return arity of every Move call the put-exercise PTBs can emit.
fn return_arity(module: &str, function: &str) -> Option<usize> {
    Some(match (module, function) {
        ("coin", "zero") => 1,
        ("pool", "borrow_flashloan_base") | ("pool", "borrow_flashloan_quote") => 2,
        ("pool", "return_flashloan_base") | ("pool", "return_flashloan_quote") => 0,
        ("pool", "swap_exact_quote_for_base") => 3,
        ("put_bucket", "exercise") => 1,
        ("vault_mm", "exercise_put_coin") => 0,
        ("deepbook_adapter", "taker_swap_quote_for_base") => 0,
        _ => return None,
    })
}

/// Move calls yield `Result` for one value and `NestedResult`s for a
/// tuple; `SplitCoins` always yields `NestedResult`s (the builder's
/// `nested` helper indexes into it even for one amount).
fn produced(cmd_index: usize, arity: usize, always_nested: bool, out: &mut Vec<Argument>) {
    match arity {
        0 => {}
        1 if !always_nested => out.push(Argument::Result(cmd_index as u16)),
        n => out.extend((0..n as u16).map(|i| Argument::NestedResult(cmd_index as u16, i))),
    }
}

/// Every value a command produces (coins, hot potatoes) must be consumed
/// by a later command — by value into a Move call, merged, or
/// transferred — and consumed exactly once. `SplitCoins`/`MergeCoins`
/// targets are borrowed, not consumed, so the original coin must still
/// end somewhere. Returns the ordered `module::function` names for golden
/// assertions.
pub fn assert_nothing_stranded(ptb: &ProgrammableTransaction) -> Vec<String> {
    let mut outstanding: Vec<Argument> = Vec::new();
    let mut consumed: Vec<Argument> = Vec::new();
    let mut calls = Vec::new();
    let consume = |arg: Argument, outstanding: &mut Vec<Argument>, consumed: &mut Vec<Argument>| {
        if !matches!(arg, Argument::Result(_) | Argument::NestedResult(..)) {
            return;
        }
        assert!(!consumed.contains(&arg), "{arg:?} consumed twice");
        let pos = outstanding
            .iter()
            .position(|a| *a == arg)
            .unwrap_or_else(|| panic!("{arg:?} consumed but never produced"));
        outstanding.remove(pos);
        consumed.push(arg);
    };
    for (i, cmd) in ptb.commands.iter().enumerate() {
        match cmd {
            Command::MoveCall(c) => {
                let module = c.module.as_str();
                let function = c.function.as_str();
                calls.push(format!("{module}::{function}"));
                let arity = return_arity(module, function)
                    .unwrap_or_else(|| panic!("unknown return arity for {module}::{function}"));
                for a in &c.arguments {
                    consume(*a, &mut outstanding, &mut consumed);
                }
                produced(i, arity, false, &mut outstanding);
            }
            Command::SplitCoins(_coin, amounts) => {
                calls.push("SplitCoins".into());
                produced(i, amounts.len(), true, &mut outstanding);
            }
            Command::MergeCoins(_into, sources) => {
                calls.push("MergeCoins".into());
                for a in sources {
                    consume(*a, &mut outstanding, &mut consumed);
                }
            }
            Command::TransferObjects(objs, _) => {
                calls.push("TransferObjects".into());
                for a in objs {
                    consume(*a, &mut outstanding, &mut consumed);
                }
            }
            other => panic!("unexpected command in put-exercise PTB: {other:?}"),
        }
    }
    assert!(outstanding.is_empty(), "stranded results: {outstanding:?}");
    calls
}

/// Decode the `u64` behind a pure input argument (test helper).
pub fn pure_u64(ptb: &ProgrammableTransaction, arg: Argument) -> Option<u64> {
    let Argument::Input(i) = arg else { return None };
    match ptb.inputs.get(i as usize)? {
        CallArg::Pure(bytes) => bcs::from_bytes(bytes).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_types::base_types::SequenceNumber;
    use sui_types::transaction::{ObjectArg, SharedObjectMutability};

    const U: &str = "0xa::tsui::TSUI";
    const S: &str = "0xb::tusdc::TUSDC";
    const P: &str = "0xc::put_3::PUT_3";
    const DEEP: &str = "0xd::deep::DEEP";

    fn spec() -> PutPtbSpec<'static> {
        PutPtbSpec {
            deepbook_package: ObjectID::from_hex_literal("0xdb").unwrap(),
            core_package: ObjectID::from_hex_literal("0xc0").unwrap(),
            underlying_type: U,
            settlement_type: S,
            put_coin_type: P,
            deep_coin_type: DEEP,
            amount: 5_000_000_000,
            payout: 20_000_000,
            max_quote_in: 18_000_000,
            min_profit: 1_000_000,
            recipient: SuiAddress::ZERO,
        }
    }

    fn shared(pt: &mut ProgrammableTransactionBuilder, id: u8) -> Argument {
        pt.obj(ObjectArg::SharedObject {
            id: ObjectID::from_single_byte(id),
            initial_shared_version: SequenceNumber::from_u64(1),
            mutability: SharedObjectMutability::Mutable,
        })
        .unwrap()
    }

    fn args(pt: &mut ProgrammableTransactionBuilder, with_underlying: bool) -> PutPtbArgs {
        let pool = shared(pt, 1);
        let bucket = shared(pt, 2);
        let clock = crate::tx::clock_arg(pt).unwrap();
        // Coin inputs stand in for gathered wallet coins.
        let puts = shared(pt, 3);
        let underlying = with_underlying.then(|| shared(pt, 4));
        PutPtbArgs { pool, bucket, clock, puts, underlying }
    }

    fn build(path: PutPath, spec: &PutPtbSpec<'_>) -> ProgrammableTransaction {
        let mut pt = ProgrammableTransactionBuilder::new();
        let a = args(&mut pt, path == PutPath::VaultUnderlying);
        build_put_exercise(&mut pt, spec, &a, path).unwrap();
        pt.finish()
    }

    fn move_call(
        ptb: &ProgrammableTransaction,
        i: usize,
    ) -> &sui_types::transaction::ProgrammableMoveCall {
        match &ptb.commands[i] {
            Command::MoveCall(c) => c,
            other => panic!("command {i} is not a move call: {other:?}"),
        }
    }

    fn split_amount(ptb: &ProgrammableTransaction, i: usize) -> u64 {
        match &ptb.commands[i] {
            Command::SplitCoins(_, amts) => pure_u64(ptb, amts[0]).unwrap(),
            other => panic!("command {i} is not a split: {other:?}"),
        }
    }

    #[test]
    fn vault_underlying_path_golden() {
        let ptb = build(PutPath::VaultUnderlying, &spec());
        let calls = assert_nothing_stranded(&ptb);
        assert_eq!(
            calls,
            [
                "put_bucket::exercise",
                "SplitCoins",
                "coin::zero",
                "pool::swap_exact_quote_for_base",
                "SplitCoins",
                "TransferObjects",
            ]
        );
        // Explicit max-input on the repurchase, min-output = amount, and
        // the on-chain minimum-profit split.
        assert_eq!(split_amount(&ptb, 1), 18_000_000);
        let swap = move_call(&ptb, 3);
        assert_eq!(pure_u64(&ptb, swap.arguments[3]), Some(5_000_000_000));
        assert_eq!(split_amount(&ptb, 4), 1_000_000);
    }

    #[test]
    fn base_flash_path_golden() {
        let ptb = build(PutPath::BaseFlash, &spec());
        let calls = assert_nothing_stranded(&ptb);
        assert_eq!(
            calls,
            [
                "pool::borrow_flashloan_base",
                "put_bucket::exercise",
                "SplitCoins",
                "coin::zero",
                "pool::swap_exact_quote_for_base",
                "SplitCoins",
                "pool::return_flashloan_base",
                "SplitCoins",
                "TransferObjects",
            ]
        );
        // Exact repayment: the borrow amount equals the repay split.
        let borrow = move_call(&ptb, 0);
        assert_eq!(pure_u64(&ptb, borrow.arguments[1]), Some(5_000_000_000));
        assert_eq!(split_amount(&ptb, 5), 5_000_000_000);
        let repay = move_call(&ptb, 6);
        assert_eq!(repay.arguments[1], Argument::NestedResult(5, 0));
        assert_eq!(repay.arguments[2], Argument::NestedResult(0, 1)); // the hot potato
        assert_eq!(split_amount(&ptb, 2), 18_000_000);
        assert_eq!(split_amount(&ptb, 7), 1_000_000);
    }

    #[test]
    fn quote_flash_path_golden() {
        let ptb = build(PutPath::QuoteFlash, &spec());
        let calls = assert_nothing_stranded(&ptb);
        assert_eq!(
            calls,
            [
                "pool::borrow_flashloan_quote",
                "coin::zero",
                "pool::swap_exact_quote_for_base",
                "SplitCoins",
                "put_bucket::exercise",
                "MergeCoins",
                "SplitCoins",
                "pool::return_flashloan_quote",
                "SplitCoins",
                "TransferObjects",
            ]
        );
        let borrow = move_call(&ptb, 0);
        assert_eq!(pure_u64(&ptb, borrow.arguments[1]), Some(18_000_000));
        // Delivery is exactly `amount`; repayment exactly the principal.
        assert_eq!(split_amount(&ptb, 3), 5_000_000_000);
        assert_eq!(split_amount(&ptb, 6), 18_000_000);
        let repay = move_call(&ptb, 7);
        assert_eq!(repay.arguments[1], Argument::NestedResult(6, 0));
        assert_eq!(repay.arguments[2], Argument::NestedResult(0, 1));
        assert_eq!(split_amount(&ptb, 8), 1_000_000);
    }

    #[test]
    fn profit_bound_refuses_before_any_command() {
        let mut s = spec();
        s.max_quote_in = 19_500_000; // + 1_000_000 min profit > 20_000_000 payout
        for path in PutPath::ORDER {
            let mut pt = ProgrammableTransactionBuilder::new();
            let a = args(&mut pt, true);
            let err = build_put_exercise(&mut pt, &s, &a, path).unwrap_err();
            assert!(err.to_string().contains("profit bound"), "{path:?}: {err}");
            assert!(pt.finish().commands.is_empty(), "{path:?} emitted commands");
        }
    }

    #[test]
    fn vault_underlying_path_needs_the_delivery_coin() {
        let mut pt = ProgrammableTransactionBuilder::new();
        let a = args(&mut pt, false);
        assert!(build_put_exercise(&mut pt, &spec(), &a, PutPath::VaultUnderlying).is_err());
    }

    #[test]
    fn vault_custody_path_golden() {
        let s = VaultPutPtbSpec {
            trading_vault_package: ObjectID::from_hex_literal("0x71").unwrap(),
            deepbook_adapter_package: ObjectID::from_hex_literal("0xad").unwrap(),
            underlying_type: U,
            settlement_type: S,
            put_coin_type: P,
            coin_position_id: ObjectID::from_hex_literal("0x99").unwrap(),
            amount: 5_000_000_000,
            payout: 20_000_000,
            max_quote_in: 18_000_000,
            min_profit: 1_000_000,
        };
        let mut pt = ProgrammableTransactionBuilder::new();
        let a = VaultPutPtbArgs {
            vault: shared(&mut pt, 1),
            cap: shared(&mut pt, 2),
            reg: shared(&mut pt, 3),
            allowlist: shared(&mut pt, 4),
            pool: shared(&mut pt, 5),
            bucket: shared(&mut pt, 6),
            clock: crate::tx::clock_arg(&mut pt).unwrap(),
        };
        build_vault_put_exercise(&mut pt, &s, &a).unwrap();
        let ptb = pt.finish();
        let calls = assert_nothing_stranded(&ptb);
        assert_eq!(
            calls,
            ["vault_mm::exercise_put_coin", "deepbook_adapter::taker_swap_quote_for_base"]
        );
        let swap = move_call(&ptb, 1);
        assert_eq!(swap.arguments[3], a.allowlist); // pool allowlist enforced on-chain
        assert_eq!(pure_u64(&ptb, swap.arguments[5]), Some(18_000_000));
        assert_eq!(pure_u64(&ptb, swap.arguments[6]), Some(5_000_000_000));

        let mut bad = s.clone();
        bad.max_quote_in = 19_500_000;
        let mut pt = ProgrammableTransactionBuilder::new();
        assert!(build_vault_put_exercise(&mut pt, &bad, &a).is_err());
    }

    #[test]
    fn stranded_checker_catches_a_dropped_hot_potato() {
        // A base-flash PTB with the repayment removed: the FlashLoan and
        // the borrowed coin are never consumed.
        let s = spec();
        let mut pt = ProgrammableTransactionBuilder::new();
        let a = args(&mut pt, false);
        let amt = pt.pure(&s.amount).unwrap();
        pool_call(&mut pt, &s, "borrow_flashloan_base", vec![a.pool, amt]).unwrap();
        let ptb = pt.finish();
        let r = std::panic::catch_unwind(|| assert_nothing_stranded(&ptb));
        assert!(r.is_err());
    }
}
