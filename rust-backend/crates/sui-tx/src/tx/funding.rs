//! Coin arguments funded from whatever a wallet holds.
//!
//! A wallet's funds live in two places: `Coin<T>` objects, and the *address
//! balance* the accumulator holds for it. Faucet drips and plain transfers
//! land in the address balance, which is invisible to a `Coin<T>` read and
//! cannot be passed to Move directly — it has to be withdrawn first, with a
//! `CallArg::FundsWithdrawal` reservation redeemed by
//! `0x2::coin::redeem_funds<T>`.
//!
//! The builders here do that inline, in the same PTB that needs the coin, so
//! nothing ever needs a separate redeem transaction: coin objects are spent
//! first (and merged, which compacts dust as a side effect), and only the
//! shortfall is withdrawn from the address balance.
//!
//! One caveat for `Coin<SUI>` specifically: these builders name coin objects
//! as transaction *inputs*, and the object gas selection picks cannot also be
//! an input. Every caller today funds a non-SUI asset (DEEP, option coins,
//! settlement), where the two can't collide. A SUI amount inside a PTB should
//! come from `Argument::GasCoin` where there is a gas coin — see
//! `pyth_update::update_fees`, which picks between the two by asking
//! [`crate::chain::ChainClient::gas_payment`] the same question the submission
//! will.

use anyhow::{anyhow, bail, Context, Result};
use move_core_types::language_storage::StructTag;
use sui_types::base_types::{ObjectRef, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{
    Argument, Command, FundsWithdrawalArg, ObjectArg, ProgrammableTransaction,
};
use sui_types::{Identifier, TypeTag, SUI_FRAMEWORK_PACKAGE_ID};

use crate::chain::ChainClient;

/// One `Coin<T>` argument holding exactly `amount`.
pub async fn exact_coin(
    client: &ChainClient,
    owner: SuiAddress,
    pt: &mut ProgrammableTransactionBuilder,
    coin_type: &StructTag,
    amount: u64,
) -> Result<Argument> {
    let mut coins = exact_coins(client, owner, pt, coin_type, amount, 1).await?;
    Ok(coins.remove(0))
}

/// `count` `Coin<T>` arguments of exactly `amount` each, funded from the
/// wallet's coin objects, its address balance, or both.
///
/// The leftover is deliberately never a PTB result. Coins have no `drop`, so a
/// result left unused aborts the transaction — where the change lands is a
/// correctness question, not a tidiness one:
///
/// * with coin objects, the split runs against the merged *input* object, and
///   its remainder simply stays in that object, owned by `owner` as before;
/// * with no coin objects at all, we reserve exactly `amount * count` and hand
///   back the withdrawn coin itself as the last slice, so nothing is left over
///   to strand.
pub async fn exact_coins(
    client: &ChainClient,
    owner: SuiAddress,
    pt: &mut ProgrammableTransactionBuilder,
    coin_type: &StructTag,
    amount: u64,
    count: usize,
) -> Result<Vec<Argument>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let needed = (amount as u128)
        .checked_mul(count as u128)
        .and_then(|n| u64::try_from(n).ok())
        .ok_or_else(|| anyhow!("{count} × {amount} of {coin_type} overflows u64"))?;

    let coins = client
        .coins(owner, coin_type)
        .await
        .with_context(|| format!("listing {coin_type} coins for {owner}"))?;
    let from_coins: u64 = coins
        .iter()
        .map(|c| c.balance)
        .fold(0u64, |a, b| a.saturating_add(b));

    // Only the part the coin objects can't cover is withdrawn, so a wallet
    // that still has coins keeps using them.
    let shortfall = needed.saturating_sub(from_coins);
    if shortfall > 0 {
        let available = client
            .address_balance(owner, coin_type)
            .await
            .with_context(|| format!("reading {owner}'s {coin_type} address balance"))?;
        if available < shortfall {
            bail!(
                "{owner} holds {from_coins} of {coin_type} in coins and {available} as an \
                 address balance, need {needed}"
            );
        }
    }

    let refs: Vec<_> = coins.iter().map(|c| c.object_ref).collect();
    emit(pt, coin_type, &refs, shortfall, amount, count)
}

/// The PTB half of [`exact_coins`], once the sources are known: spend `refs`,
/// withdraw `shortfall` (zero for none), hand back `count` coins of `amount`.
fn emit(
    pt: &mut ProgrammableTransactionBuilder,
    coin_type: &StructTag,
    refs: &[ObjectRef],
    shortfall: u64,
    amount: u64,
    count: usize,
) -> Result<Vec<Argument>> {
    let withdrawn = (shortfall > 0)
        .then(|| redeem(pt, coin_type, shortfall))
        .transpose()?;

    let Some(first) = refs.first() else {
        // No coin objects: the withdrawal is the whole funding, and it is
        // exact, so the last slice is the withdrawn coin itself. Splitting it
        // `count` times instead would leave a zero-value result behind, and a
        // Coin result nobody consumes has no `drop` — the transaction aborts.
        let withdrawn = withdrawn.expect("shortfall is the full amount when there are no coins");
        if count == 1 {
            return Ok(vec![withdrawn]);
        }
        let amt = pt.pure(&amount)?;
        let split = pt.command(Command::SplitCoins(withdrawn, vec![amt; count - 1]));
        let mut out = nested_results(split, count - 1)?;
        out.push(withdrawn);
        return Ok(out);
    };

    // Everything merges into the first coin object, which keeps the change:
    // an input object's remainder stays owned by the sender, so no leftover
    // needs a home.
    let primary = pt.obj(ObjectArg::ImmOrOwnedObject(*first))?;
    let mut rest: Vec<Argument> = refs[1..]
        .iter()
        .map(|r| pt.obj(ObjectArg::ImmOrOwnedObject(*r)))
        .collect::<Result<_, _>>()?;
    rest.extend(withdrawn);
    if !rest.is_empty() {
        pt.command(Command::MergeCoins(primary, rest));
    }
    // One amount Argument reused `count` times (pure inputs aren't consumed).
    let amt = pt.pure(&amount)?;
    let split = pt.command(Command::SplitCoins(primary, vec![amt; count]));
    nested_results(split, count)
}

/// A PTB that moves `amount` of `owner`'s address balance into a `Coin<T>`
/// object owned by `owner`.
///
/// Nothing in this workspace needs a materialised coin to *call* Move any
/// more — [`exact_coins`] withdraws inline. This is for the one case that
/// genuinely needs the object itself: sponsoring, where the gas payment must
/// name coins belonging to the sponsor (see the gas-station).
pub fn redeem_to_coin(
    owner: SuiAddress,
    coin_type: &StructTag,
    amount: u64,
) -> Result<ProgrammableTransaction> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let coin = redeem(&mut pt, coin_type, amount)?;
    pt.transfer_arg(owner, coin);
    Ok(pt.finish())
}

/// Reserve `amount` of the sender's address balance and redeem it into a
/// `Coin<T>` value: `0x2::coin::redeem_funds<T>(Withdrawal<Balance<T>>)`.
fn redeem(
    pt: &mut ProgrammableTransactionBuilder,
    coin_type: &StructTag,
    amount: u64,
) -> Result<Argument> {
    let tag = TypeTag::Struct(Box::new(coin_type.clone()));
    let withdrawal = pt
        .funds_withdrawal(FundsWithdrawalArg::balance_from_sender(amount, tag.clone()))
        .context("reserving an address-balance withdrawal")?;
    Ok(pt.programmable_move_call(
        SUI_FRAMEWORK_PACKAGE_ID,
        Identifier::new("coin").unwrap(),
        Identifier::new("redeem_funds").unwrap(),
        vec![tag],
        vec![withdrawal],
    ))
}

/// The `count` coins a `SplitCoins` command produced.
fn nested_results(split: Argument, count: usize) -> Result<Vec<Argument>> {
    let Argument::Result(i) = split else {
        bail!("SplitCoins returned unexpected argument {split:?}");
    };
    Ok((0..count as u16).map(|j| Argument::NestedResult(i, j)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_types::base_types::{ObjectDigest, ObjectID, SequenceNumber};
    use sui_types::transaction::CallArg;

    fn obj_ref() -> ObjectRef {
        (
            ObjectID::random(),
            SequenceNumber::from_u64(1),
            ObjectDigest::random(),
        )
    }

    fn deep() -> StructTag {
        sui_types::parse_sui_struct_tag("0xdee9::deep::DEEP").unwrap()
    }

    /// Names of the commands emitted, for shape assertions.
    fn commands(pt: ProgrammableTransactionBuilder) -> Vec<String> {
        pt.finish()
            .commands
            .iter()
            .map(|c| match c {
                Command::MoveCall(m) => format!("{}::{}", m.module, m.function),
                Command::MergeCoins(..) => "MergeCoins".to_owned(),
                Command::SplitCoins(..) => "SplitCoins".to_owned(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn coins_alone_never_withdraw() {
        let mut pt = ProgrammableTransactionBuilder::new();
        let out = emit(&mut pt, &deep(), &[obj_ref(), obj_ref()], 0, 500, 1).unwrap();
        assert_eq!(out, vec![Argument::NestedResult(1, 0)]);
        assert_eq!(commands(pt), ["MergeCoins", "SplitCoins"]);
    }

    #[test]
    fn a_shortfall_is_withdrawn_and_merged_into_the_coins() {
        let mut pt = ProgrammableTransactionBuilder::new();
        let out = emit(&mut pt, &deep(), &[obj_ref()], 200, 500, 1).unwrap();
        assert_eq!(out, vec![Argument::NestedResult(2, 0)]);
        assert_eq!(commands(pt), ["coin::redeem_funds", "MergeCoins", "SplitCoins"]);
    }

    /// The wallet the whole change exists for: no coin objects at all.
    #[test]
    fn a_lone_withdrawal_is_returned_whole() {
        let mut pt = ProgrammableTransactionBuilder::new();
        let out = emit(&mut pt, &deep(), &[], 500, 500, 1).unwrap();
        // Redeemed exactly, so it is handed back as-is — no split, and
        // therefore nothing left over.
        assert_eq!(out, vec![Argument::Result(0)]);
        assert_eq!(commands(pt), ["coin::redeem_funds"]);
    }

    /// A `Coin` result nobody consumes aborts the transaction, so the last
    /// slice of a coin-less funding must be the withdrawn coin itself rather
    /// than an nth split leaving a zero-value remainder.
    #[test]
    fn a_lone_withdrawal_strands_nothing_when_split() {
        let mut pt = ProgrammableTransactionBuilder::new();
        let out = emit(&mut pt, &deep(), &[], 1_500, 500, 3).unwrap();
        assert_eq!(
            out,
            vec![
                Argument::NestedResult(1, 0),
                Argument::NestedResult(1, 1),
                // ...the redeemed coin, now holding exactly one slice.
                Argument::Result(0),
            ]
        );
        assert_eq!(commands(pt), ["coin::redeem_funds", "SplitCoins"]);
    }

    #[test]
    fn the_withdrawal_reserves_exactly_the_shortfall() {
        let mut pt = ProgrammableTransactionBuilder::new();
        emit(&mut pt, &deep(), &[], 750, 750, 1).unwrap();
        let reservations: Vec<_> = pt
            .finish()
            .inputs
            .iter()
            .filter_map(|i| match i {
                CallArg::FundsWithdrawal(w) => Some(w.reservation.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reservations,
            vec![sui_types::transaction::Reservation::MaxAmountU64(750)]
        );
    }
}
