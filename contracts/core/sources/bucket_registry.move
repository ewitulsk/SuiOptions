/// Deterministic bucket addressing for any-strike creation.
///
/// Buckets created through the permissionless path claim their `UID` from
/// `derived_object` under this shared registry, keyed by the bucket's full
/// economic spec. Two consequences:
///
///  1. **The bucket's object ID is computable off-chain before it exists**
///     (`derived_object::derive_address(registry_id, key)`), so an MM can
///     sign a quote binding `bucket_id` for a bucket the taker's own
///     transaction is about to create — the quote protocol is unchanged.
///  2. **One bucket per spec**, enforced by `derived_object::claim`
///     aborting on a second claim (independently of the currency
///     registry's one-coin-per-type guarantee).
module options_core::bucket_registry;

use std::type_name::TypeName;
use sui::derived_object;

public struct BucketRegistry has key { id: UID }

/// The full economic identity of a bucket. `sig`/`exp` are the NORMALIZED
/// strike (see `option_coin::normalize_strike`), so equivalent raw
/// (strike, scale) inputs collapse to one key.
public struct BucketKey has copy, drop, store {
    asset: TypeName,
    settlement: TypeName,
    expiry_ms: u64,
    sig: u64,
    exp: u8,
    is_put: bool,
}

fun init(ctx: &mut TxContext) {
    transfer::share_object(BucketRegistry { id: object::new(ctx) });
}

/// The id-leak verifier requires `derived_object::claim` to appear
/// DIRECTLY in the function that constructs the object, so the bucket
/// modules claim inline; this module only lends its `UID` and builds keys.
public(package) fun uid_mut(registry: &mut BucketRegistry): &mut UID {
    &mut registry.id
}

public(package) fun key(
    asset: TypeName,
    settlement: TypeName,
    expiry_ms: u64,
    sig: u64,
    exp: u8,
    is_put: bool,
): BucketKey {
    BucketKey { asset, settlement, expiry_ms, sig, exp, is_put }
}

#[test_only]
public fun new_for_testing(ctx: &mut TxContext): BucketRegistry {
    BucketRegistry { id: object::new(ctx) }
}

#[test_only]
public fun share_for_testing(registry: BucketRegistry) {
    transfer::share_object(registry);
}
