module options_protocol::org;

use std::string::String;
use std::type_name;
use sui::balance::Balance;
use sui::coin::{Self, Coin};
use sui::dynamic_field as df;

use options_protocol::errors;
use options_protocol::events;

/// Same ceiling as the protocol fee. Combined worst case on a write is
/// therefore 20% of gross premium — net_premium can never underflow.
const MAX_ORG_FEE_BPS: u64 = 1000;
const MAX_ORG_NAME_BYTES: u64 = 64;

/// Shared object owned by no one; all authority flows through the OrgCap.
/// The Org IS its own treasury: per-asset fee balances live as dynamic
/// fields keyed by `BalanceKey<T>` (same pattern as account/treasury).
public struct Org has key {
    id: UID,
    /// Cosmetic label. Not unique — `org_id` is the identity.
    name: String,
    /// Org-set fee in basis points, skimmed from gross premium on every
    /// write into this org's buckets (alongside the protocol fee).
    fee_bps: u64,
}

/// Sole authority over the org: fee setting, fee withdrawal, and bucket
/// lifecycle (create/invalidate/revalidate/cleanup). `store` is intentional
/// — transferring the cap transfers the org (fee stream + bucket admin) as
/// a sellable unit. A lost cap permanently strands the org's fee balances;
/// the protocol AdminCap can still invalidate/revalidate its buckets.
public struct OrgCap has key, store {
    id: UID,
    org_id: ID,
}

public struct BalanceKey<phantom T> has copy, drop, store {}

/// Permissionless. Shares the Org and returns the cap to the calling PTB.
public fun create_org(name: String, fee_bps: u64, ctx: &mut TxContext): OrgCap {
    assert!(fee_bps <= MAX_ORG_FEE_BPS, errors::fee_too_high());
    let name_bytes = name.length();
    assert!(name_bytes > 0 && name_bytes <= MAX_ORG_NAME_BYTES, errors::org_name_invalid());

    let org = Org {
        id: object::new(ctx),
        name,
        fee_bps,
    };
    let org_id = object::id(&org);
    let cap = OrgCap { id: object::new(ctx), org_id };
    events::emit_org_created(org_id, org.name, fee_bps, ctx.sender());
    transfer::share_object(org);
    cap
}

public fun set_org_fee_bps(cap: &OrgCap, org: &mut Org, new_bps: u64) {
    assert_cap(cap, object::id(org));
    assert!(new_bps <= MAX_ORG_FEE_BPS, errors::fee_too_high());
    let old_bps = org.fee_bps;
    org.fee_bps = new_bps;
    events::emit_org_fee_updated(object::id(org), old_bps, new_bps);
}

public fun withdraw<T>(
    cap: &OrgCap,
    org: &mut Org,
    amount: u64,
    ctx: &mut TxContext,
): Coin<T> {
    assert_cap(cap, object::id(org));
    let key = BalanceKey<T> {};
    assert!(df::exists(&org.id, key), errors::insufficient_org_balance());
    let bal: &mut Balance<T> = df::borrow_mut(&mut org.id, key);
    assert!(bal.value() >= amount, errors::insufficient_org_balance());
    let out = coin::from_balance(bal.split(amount), ctx);
    events::emit_org_withdraw(object::id(org), type_name::with_defining_ids<T>(), amount);
    out
}

public(package) fun deposit_balance<T>(org: &mut Org, bal_in: Balance<T>) {
    let key = BalanceKey<T> {};
    if (df::exists(&org.id, key)) {
        let bal: &mut Balance<T> = df::borrow_mut(&mut org.id, key);
        bal.join(bal_in);
    } else {
        df::add(&mut org.id, key, bal_in);
    };
}

public(package) fun assert_cap(cap: &OrgCap, org_id: ID) {
    assert!(cap.org_id == org_id, errors::org_cap_mismatch());
}

public fun fee_bps(org: &Org): u64 { org.fee_bps }

public fun name(org: &Org): &String { &org.name }

public fun cap_org_id(cap: &OrgCap): ID { cap.org_id }

public fun balance_of<T>(org: &Org): u64 {
    let key = BalanceKey<T> {};
    if (!df::exists(&org.id, key)) {
        return 0
    };
    let bal: &Balance<T> = df::borrow(&org.id, key);
    bal.value()
}
