#[allow(deprecated_usage)]
module test_tokens::tdeep;

use sui::coin::{Self, Coin, TreasuryCap};

public struct TDEEP has drop {}

public struct Faucet has key {
    id: UID,
    cap: TreasuryCap<TDEEP>,
}

fun init(witness: TDEEP, ctx: &mut TxContext) {
    let (cap, metadata) = coin::create_currency<TDEEP>(
        witness,
        6,
        b"TDEEP",
        b"Test DEEP",
        b"Faucet-mintable test DEEP (DeepBook) for the options-protocol",
        option::none(),
        ctx,
    );
    transfer::public_freeze_object(metadata);
    let faucet = Faucet { id: object::new(ctx), cap };
    transfer::share_object(faucet);
}

public fun mint(faucet: &mut Faucet, amount: u64, ctx: &mut TxContext): Coin<TDEEP> {
    coin::mint(&mut faucet.cap, amount, ctx)
}

#[allow(lint(self_transfer))]
public fun mint_to_sender(faucet: &mut Faucet, amount: u64, ctx: &mut TxContext) {
    let coin = mint(faucet, amount, ctx);
    transfer::public_transfer(coin, ctx.sender());
}
