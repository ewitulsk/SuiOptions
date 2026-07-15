/// Protocol entry point for outbound Circle CCTP v1 transfers.
///
/// Circle's `deposit_for_burn` is an `entry fun`, so dependent packages cannot
/// call it directly. Circle's documented integration pattern is the ticket
/// flow: this module builds a `DepositForBurnTicket` authenticated with our
/// package-private `BridgeAuth`, and the same PTB passes the ticket to
/// `token_messenger_minter::deposit_for_burn::deposit_for_burn_with_package_auth`
/// (version-gated on Circle's side, so it is called from the PTB rather than
/// from this module — that keeps this package working across CCTP upgrades).
///
/// PTB shape (see frontend/src/tx/bridge.ts):
///   1. coin = coinWithBalance(USDC, amount)
///   2. ticket = cctp_bridge::bridge::prepare_deposit_for_burn(coin, domain, recipient)
///   3. token_messenger_minter::deposit_for_burn::deposit_for_burn_with_package_auth(
///        ticket, tmm_state, mt_state, deny_list(0x403), treasury)
module cctp_bridge::bridge {
    use std::ascii::String;
    use std::type_name;
    use sui::coin::Coin;
    use sui::event;
    use token_messenger_minter::deposit_for_burn::{Self, DepositForBurnTicket};

    /// Only this module can construct a `BridgeAuth`, so every ticket built
    /// here carries this package's auth identifier as the CCTP message sender.
    public struct BridgeAuth has drop {}

    public struct BridgeInitiated has copy, drop {
        sender: address,
        amount: u64,
        destination_domain: u32,
        mint_recipient: address,
        coin_type: String,
    }

    /// Burn-side entry point: emits `BridgeInitiated` and returns the burn
    /// ticket the PTB must hand to Circle's `deposit_for_burn_with_package_auth`.
    public fun prepare_deposit_for_burn<T: drop>(
        coins: Coin<T>,
        destination_domain: u32,
        mint_recipient: address,
        ctx: &TxContext,
    ): DepositForBurnTicket<T, BridgeAuth> {
        event::emit(BridgeInitiated {
            sender: ctx.sender(),
            amount: coins.value(),
            destination_domain,
            mint_recipient,
            coin_type: type_name::with_defining_ids<T>().into_string(),
        });
        deposit_for_burn::create_deposit_for_burn_ticket(
            BridgeAuth {},
            coins,
            destination_domain,
            mint_recipient,
        )
    }

    #[test_only]
    public fun bridge_initiated_for_testing(
        sender: address,
        amount: u64,
        destination_domain: u32,
        mint_recipient: address,
        coin_type: String,
    ): BridgeInitiated {
        BridgeInitiated { sender, amount, destination_domain, mint_recipient, coin_type }
    }
}
