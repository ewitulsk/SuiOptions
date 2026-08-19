/// The oracle abstraction (design doc §4.1). Vault core never touches an
/// oracle SDK: appraisal consumes `PriceAttestation`s, and the only mint
/// path is `attest`, gated on an allowlisted oracle-adapter witness. The
/// first adapter is `contracts/oracle-pyth`; swapping or adding sources
/// is a new package plus one `OracleRegistry` entry.
module vault_v2::price;

use std::type_name::{Self, TypeName};

use vault_v2::errors;
use vault_v2::registry::{Self, OracleRegistry};

/// Fixed point for `price`: `value_quote_raw = amount_asset_raw × price /
/// 10^12`. Decimals are the ADAPTER's job to bake in — the price is a
/// raw-smallest-unit conversion ratio, the same shape as
/// `options_vault::oracle::spot_cross` output at scale 12.
const PRICE_SCALE: u128 = 1_000_000_000_000;

/// Transient, in-transaction price statement: `asset` → `quote_asset` at
/// `price`/`PRICE_SCALE`, published at `timestamp_ms` (source time, not
/// chain time). `copy, drop` — never stored; core additionally enforces
/// its `max_price_age_ms` backstop at consumption.
public struct PriceAttestation has copy, drop {
    oracle: TypeName,
    asset: TypeName,
    quote_asset: TypeName,
    price: u128,
    timestamp_ms: u64,
}

/// The only mint path, witness-gated on the oracle allowlist and — when
/// the asset carries one — on its per-asset oracle pin (SO-335). The two
/// checks abort distinctly so an operator can tell "this adapter is
/// delisted" from "this adapter is fine, just not for this asset".
public fun attest<W: drop>(
    _witness: W,
    reg: &OracleRegistry,
    asset: TypeName,
    quote_asset: TypeName,
    price: u128,
    timestamp_ms: u64,
): PriceAttestation {
    let oracle = type_name::with_defining_ids<W>();
    assert!(registry::is_oracle_allowed(reg, &oracle), errors::oracle_not_allowed());
    assert!(
        registry::is_oracle_allowed_for(reg, &oracle, &asset),
        errors::oracle_not_pinned_for_asset(),
    );
    assert!(price > 0, errors::price_invalid());
    PriceAttestation { oracle, asset, quote_asset, price, timestamp_ms }
}

public fun price_scale(): u128 { PRICE_SCALE }

public fun oracle(a: &PriceAttestation): TypeName { a.oracle }

public fun asset(a: &PriceAttestation): TypeName { a.asset }

public fun quote_asset(a: &PriceAttestation): TypeName { a.quote_asset }

public fun price(a: &PriceAttestation): u128 { a.price }

public fun timestamp_ms(a: &PriceAttestation): u64 { a.timestamp_ms }
