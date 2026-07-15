/// Example scoped entrypoint (spec §1.6). A real dApp (see `options_protocol`)
/// defines `_with_session` variants of its own functions, each declaring its
/// full selector and delegating the cap checks to `session::authorize` /
/// `session::authorize_spend`.
module siws_session::app_example;

use sui::clock::Clock;

use siws_session::account::Account;
use siws_session::session::{Self, SessionCap};

/// Full `pkg::module::function` selector the SDK must include in `allowed`.
const SEL_WITHDRAW: vector<u8> = b"siws_session::app_example::withdraw";

/// Move `amount` of the account's `T` balance to `recipient`, gated by the cap
/// (including the cap's per-type spend limit for `T`).
public entry fun withdraw<T>(
    cap: &SessionCap,
    account: &mut Account,
    clock: &Clock,
    amount: u64,
    recipient: address,
    ctx: &mut TxContext,
) {
    session::authorize_spend<T>(cap, account, clock, amount, SEL_WITHDRAW, ctx.sender());
    let coin = account.take<T>(amount, ctx);
    transfer::public_transfer(coin, recipient);
}

/// Exposed so the SDK/tests can reference the canonical selector string.
public fun withdraw_selector(): vector<u8> { SEL_WITHDRAW }
