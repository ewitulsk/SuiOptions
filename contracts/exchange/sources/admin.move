/// AdminCap (spec §4.9). Ideally held by a multisig; no admin path can move
/// user escrow or alter fill state — the cap only gates pause, fee params,
/// market listing and fee-vault sweeps.
module exchange::admin;

public struct AdminCap has key, store {
    id: UID,
}

fun init(ctx: &mut TxContext) {
    transfer::transfer(AdminCap { id: object::new(ctx) }, ctx.sender());
}

#[test_only]
public fun mint_for_testing(ctx: &mut TxContext): AdminCap {
    AdminCap { id: object::new(ctx) }
}

#[test_only]
public fun burn_for_testing(cap: AdminCap) {
    let AdminCap { id } = cap;
    id.delete();
}
