/// Session-gated twins of the covered-call vault's user flows (siws_session
/// integration — see `session_account` for the custody model).
///
/// Coins are sourced from and settle into the user's session-linked options
/// `Account`; receipt objects are custodied on it. Only `deposit` spends
/// against the cap's per-type limits — every other flow moves value that
/// remains the user's (escrowed shares / receipts redeemable only back into
/// the same custody).
///
/// The lifecycle cranks (`crank_redeem`, `swap_proceeds`, `finalize_round`,
/// `select_bucket`, `open_rfq`, `settle_rfq*`) are permissionless keeper
/// functions and `rfq::bid` is MM-facing — none of them move user custody,
/// so they get no session twins.
module options_protocol::session_vault;

use std::type_name;
use sui::clock::Clock;

use siws_session::account::Account as SessionAccount;
use siws_session::session::{Self, SessionCap};

use options_protocol::account::{Self, Account};
use options_protocol::events;
use options_protocol::session_account;
use options_protocol::vault::{Self, DepositReceipt, Vault, WithdrawReceipt};

const SEL_DEPOSIT: vector<u8> = b"options_protocol::session_vault::deposit_with_session";
const SEL_CLAIM_SHARES: vector<u8> =
    b"options_protocol::session_vault::claim_shares_with_session";
const SEL_INITIATE_WITHDRAW: vector<u8> =
    b"options_protocol::session_vault::initiate_withdraw_with_session";
const SEL_COMPLETE_WITHDRAW: vector<u8> =
    b"options_protocol::session_vault::complete_withdraw_with_session";
const SEL_INSTANT_WITHDRAW: vector<u8> =
    b"options_protocol::session_vault::instant_withdraw_pending_with_session";

public fun deposit_selector(): vector<u8> { SEL_DEPOSIT }
public fun claim_shares_selector(): vector<u8> { SEL_CLAIM_SHARES }
public fun initiate_withdraw_selector(): vector<u8> { SEL_INITIATE_WITHDRAW }
public fun complete_withdraw_selector(): vector<u8> { SEL_COMPLETE_WITHDRAW }
public fun instant_withdraw_selector(): vector<u8> { SEL_INSTANT_WITHDRAW }

/// Session twin of `vault::deposit`: underlying out of custody (charged
/// against the cap's limit for `U`), the `DepositReceipt` custodied on the
/// account.
public fun deposit_with_session<U, S, V>(
    vault: &mut Vault<U, S, V>,
    user_account: &mut Account,
    cap: &SessionCap,
    session_account: &mut SessionAccount,
    amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    session_account::assert_session_linked(user_account, cap);
    session::authorize_spend<U>(
        cap, session_account, clock, amount, SEL_DEPOSIT, ctx.sender(),
    );
    let coin = account::withdraw_internal<U>(user_account, amount, ctx);
    events::emit_account_withdraw(
        object::id(user_account),
        type_name::with_defining_ids<U>(),
        amount,
    );
    let receipt = vault::deposit(vault, coin, ctx);
    session_account::store_object(user_account, receipt);
}

/// Session twin of `vault::claim_shares`: converts a custodied receipt; the
/// share coins settle into custody. No value leaves the user —
/// `authorize`-gated.
public fun claim_shares_with_session<U, S, V>(
    vault: &mut Vault<U, S, V>,
    user_account: &mut Account,
    cap: &SessionCap,
    session_account: &SessionAccount,
    receipt_id: ID,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    session_account::assert_session_linked(user_account, cap);
    session::authorize(cap, session_account, clock, SEL_CLAIM_SHARES, ctx.sender());
    let receipt = session_account::take_object<DepositReceipt>(user_account, receipt_id);
    let shares = vault::claim_shares(vault, receipt, ctx);
    account::deposit_balance(user_account, shares.into_balance());
}

/// Session twin of `vault::initiate_withdraw`: escrows custodied shares for
/// the two-step withdrawal; the `WithdrawReceipt` is custodied. The escrowed
/// value stays the user's (redeemable only back into this custody), so this
/// is `authorize`-gated — per-vault share types can't be enumerated in a
/// cap's limits anyway.
public fun initiate_withdraw_with_session<U, S, V>(
    vault: &mut Vault<U, S, V>,
    user_account: &mut Account,
    cap: &SessionCap,
    session_account: &SessionAccount,
    shares: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    session_account::assert_session_linked(user_account, cap);
    session::authorize(cap, session_account, clock, SEL_INITIATE_WITHDRAW, ctx.sender());
    let share_coin = account::withdraw_internal<V>(user_account, shares, ctx);
    events::emit_account_withdraw(
        object::id(user_account),
        type_name::with_defining_ids<V>(),
        shares,
    );
    let receipt = vault::initiate_withdraw(vault, share_coin, ctx);
    session_account::store_object(user_account, receipt);
}

/// Session twin of `vault::complete_withdraw`: pays a finalized withdrawal
/// into custody. `authorize`-gated (funds land back in the account).
public fun complete_withdraw_with_session<U, S, V>(
    vault: &mut Vault<U, S, V>,
    user_account: &mut Account,
    cap: &SessionCap,
    session_account: &SessionAccount,
    receipt_id: ID,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    session_account::assert_session_linked(user_account, cap);
    session::authorize(cap, session_account, clock, SEL_COMPLETE_WITHDRAW, ctx.sender());
    let receipt = session_account::take_object<WithdrawReceipt>(user_account, receipt_id);
    let owed = vault::complete_withdraw(vault, receipt, ctx);
    account::deposit_balance(user_account, owed.into_balance());
}

/// Session twin of `vault::instant_withdraw_pending`: cancels a
/// not-yet-started deposit; the refund lands back in custody.
/// `authorize`-gated.
public fun instant_withdraw_pending_with_session<U, S, V>(
    vault: &mut Vault<U, S, V>,
    user_account: &mut Account,
    cap: &SessionCap,
    session_account: &SessionAccount,
    receipt_id: ID,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    session_account::assert_session_linked(user_account, cap);
    session::authorize(cap, session_account, clock, SEL_INSTANT_WITHDRAW, ctx.sender());
    let receipt = session_account::take_object<DepositReceipt>(user_account, receipt_id);
    let refund = vault::instant_withdraw_pending(vault, receipt, ctx);
    account::deposit_balance(user_account, refund.into_balance());
}
