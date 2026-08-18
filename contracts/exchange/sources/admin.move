/// AdminCap (spec §4.9). Ideally held by a multisig; no admin path can move
/// user escrow or alter fill state — the cap only gates pause, fee params,
/// market listing and fee-vault sweeps.
module exchange::admin;

public struct AdminCap has key, store {
    id: UID,
}

/// Narrow delegate: gates market listing ONLY (`registry::create_market_listed`).
/// Safe to park in a listing package's shared object — worst-case abuse is a
/// spurious market, never escrow, pause, or fee authority.
public struct ListingCap has key, store {
    id: UID,
}

fun init(ctx: &mut TxContext) {
    transfer::transfer(AdminCap { id: object::new(ctx) }, ctx.sender());
    transfer::transfer(ListingCap { id: object::new(ctx) }, ctx.sender());
}

/// Recovery path: a ListingCap wrapped in a defunct authority object is
/// otherwise unrecoverable (deploys republish rather than upgrade).
public fun mint_listing_cap(_: &AdminCap, ctx: &mut TxContext): ListingCap {
    ListingCap { id: object::new(ctx) }
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

#[test_only]
public fun mint_listing_for_testing(ctx: &mut TxContext): ListingCap {
    ListingCap { id: object::new(ctx) }
}

#[test_only]
public fun burn_listing_for_testing(cap: ListingCap) {
    let ListingCap { id } = cap;
    id.delete();
}
