#[allow(deprecated_usage)]
module test_tokens::tusdc;

use sui::coin::{Self, Coin, TreasuryCap};

public struct TUSDC has drop {}

public struct Faucet has key {
    id: UID,
    cap: TreasuryCap<TUSDC>,
}

fun init(witness: TUSDC, ctx: &mut TxContext) {
    let (cap, metadata) = coin::create_currency<TUSDC>(
        witness,
        6,
        b"TUSDC",
        b"Test USDC",
        b"Faucet-mintable test USDC for the options-protocol",
        option::none(),
        ctx,
    );
    transfer::public_freeze_object(metadata);
    let faucet = Faucet { id: object::new(ctx), cap };
    transfer::share_object(faucet);
}

public fun mint(faucet: &mut Faucet, amount: u64, ctx: &mut TxContext): Coin<TUSDC> {
    coin::mint(&mut faucet.cap, amount, ctx)
}

#[allow(lint(self_transfer))]
public fun mint_to_sender(faucet: &mut Faucet, amount: u64, ctx: &mut TxContext) {
    let coin = mint(faucet, amount, ctx);
    transfer::public_transfer(coin, ctx.sender());
}
