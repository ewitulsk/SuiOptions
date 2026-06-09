/// Example scoped entrypoint (spec §1.6). A real dApp would replace `withdraw`
/// with its own functions, each declaring its full selector and delegating the
/// cap checks to `session::authorize`.
module siws_session::app_example;

use sui::clock::Clock;

use siws_session::account::Account;
use siws_session::session::{Self, SessionCap};

/// Full `pkg::module::function` selector the SDK must include in `allowed`.
const SEL_WITHDRAW: vector<u8> = b"siws_session::app_example::withdraw";

/// Move `amount` of the account's funds to `recipient`, gated by the cap.
public entry fun withdraw<T>(
    cap: &SessionCap,
    account: &mut Account<T>,
    clock: &Clock,
    amount: u64,
    recipient: address,
    ctx: &mut TxContext,
) {
    session::authorize(cap, account, clock, amount, SEL_WITHDRAW, ctx.sender());
    let coin = account.take(amount, ctx);
    transfer::public_transfer(coin, recipient);
}

/// Exposed so the SDK/tests can reference the canonical selector string.
public fun withdraw_selector(): vector<u8> { SEL_WITHDRAW }
