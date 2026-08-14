/// Runtime option-coin currencies for any-strike buckets.
///
/// Every bucket needs a DISTINCT fungible coin type, but publishing a coin
/// package per strike (the scheduler's OTW pattern) cannot run inside a
/// user's transaction. `sui::coin_registry::new_currency` removes the OTW
/// requirement: it mints a `TreasuryCap<T>` at runtime, with one rule —
/// the call must live in the module that defines `T`'s ROOT struct (the
/// `private_generics` verifier checks only the root; instantiation type
/// arguments are free). So this module defines one generic root per side:
///
///   OptionCall<U, S, D0..D9>
///
/// and manufactures unlimited distinct coin types by instantiation. The 10
/// trailing parameters are byte markers (`enc0::B00`..`enc1::BFF`) spelling
/// the bucket's economics as a 10-byte stream, most-significant byte first:
///
///   bytes 0..4   expiry, minutes since epoch (u32)
///   bytes 4..9   strike significand (u40 — up to ~1.1e12, 13 digits)
///   byte  9      strike exponent: real strike = sig / 10^exp
///
/// Three empirically-verified Sui limits shape this exact layout:
///   • a PTB MoveCall may reference at most 15 type NODES total across its
///     type arguments (`type_input_validity_check`, counted recursively) —
///     `exercise<U, S, OptionCall<U, S, D0..D9>>` is 2 + 13 = exactly 15;
///   • datatype definitions per module cap below 256 — hence the byte
///     alphabet splits across `enc0`/`enc1` (128 structs each);
///   • sig is u40, not u64, precisely so the payload fits 10 markers.
///
/// One consequence: type-parameterized SPREAD entry points (two option
/// types + U + S ≈ 28 nodes) cannot be expressed in a PTB for any-strike
/// buckets. Spread compression stays available on legacy OTW buckets; an
/// escrow-ticket two-step for any-strike buckets is future work.
///
/// `register_call` / `register_put` take the same values as arguments,
/// rebuild the expected type string on-chain, and abort unless the
/// instantiation matches — so an encoding can never disagree with the
/// bucket it backs, and (with `normalize_strike`) every economic strike
/// has exactly ONE canonical coin type. `CoinRegistry` enforces one
/// currency per type, which doubles as bucket dedup.
///
/// Squatting is structurally impossible: a foreign module naming
/// `OptionCall` fails the publish-time verifier, and a raw PTB call to
/// `new_currency` is rejected by the transaction typing layer.
module options_core::option_coin;

use std::string::String;
use std::type_name;
use sui::coin::TreasuryCap;
use sui::coin_registry::{Self, CoinRegistry};

use options_core::enc0;
use options_core::enc1;
use options_core::errors;

/// Largest strike significand the 5-byte encoding field can carry.
const MAX_SIG: u64 = 0xFF_FFFF_FFFF; // 2^40 − 1

// ═══════════════════════════ coin type roots ═══════════════════════════

/// The call-option coin root. Never instantiated as a value — it exists
/// only as a currency type. `key` is required by `new_currency`'s bound.
public struct OptionCall<
    phantom U, phantom S,
    phantom D0, phantom D1, phantom D2, phantom D3, phantom D4,
    phantom D5, phantom D6, phantom D7, phantom D8, phantom D9,
> has key { id: UID }

/// The cash-secured-put twin.
public struct OptionPut<
    phantom U, phantom S,
    phantom D0, phantom D1, phantom D2, phantom D3, phantom D4,
    phantom D5, phantom D6, phantom D7, phantom D8, phantom D9,
> has key { id: UID }

// ═══════════════════════════ normalization ═══════════════════════════

/// Reduce a raw (strike, strike_scale) ratio to its canonical
/// (significand, exponent) form: strip trailing zeros so every economic
/// strike has exactly one representation, then require the significand to
/// fit the encoding's u40 field (13 significant digits).
public(package) fun normalize_strike(strike: u128, strike_scale: u8): (u64, u8) {
    assert!(strike > 0, errors::zero_amount());
    let mut sig = strike;
    let mut exp = strike_scale;
    while (sig % 10 == 0 && exp > 0) {
        sig = sig / 10;
        exp = exp - 1;
    };
    assert!(sig <= (MAX_SIG as u128), errors::strike_not_representable());
    ((sig as u64), exp)
}

// ═══════════════════════════ registration ═══════════════════════════

/// Register the call currency for this exact instantiation and hand back
/// its fresh `TreasuryCap`. Aborts `encoding_mismatch` unless the marker
/// parameters spell out exactly (expiry_minutes, sig, exp); aborts inside
/// `coin_registry` if the currency (⇒ the bucket economics) already
/// exists. `decimals` is display-only (callers pass the underlying's).
public(package) fun register_call<U, S, D0, D1, D2, D3, D4, D5, D6, D7, D8, D9>(
    registry: &mut CoinRegistry,
    expiry_minutes: u32,
    sig: u64,
    exp: u8,
    decimals: u8,
    ctx: &mut TxContext,
): TreasuryCap<OptionCall<U, S, D0, D1, D2, D3, D4, D5, D6, D7, D8, D9>> {
    let actual = type_name::with_defining_ids<
        OptionCall<U, S, D0, D1, D2, D3, D4, D5, D6, D7, D8, D9>,
    >().into_string().into_bytes();
    assert!(
        actual == expected_type_bytes<U, S>(b"OptionCall", expiry_minutes, sig, exp),
        errors::encoding_mismatch(),
    );
    let (init, cap) = coin_registry::new_currency<
        OptionCall<U, S, D0, D1, D2, D3, D4, D5, D6, D7, D8, D9>,
    >(
        registry,
        decimals,
        b"oCALL".to_string(),
        option_name(b"SuiOptions Call ", sig, exp, expiry_minutes),
        b"SuiOptions tokenized covered call (runtime currency)".to_string(),
        b"".to_string(),
        ctx,
    );
    // Nobody can ever edit the metadata: the coin's identity is frozen at
    // registration, authored by this module, not the transaction sender.
    coin_registry::finalize_and_delete_metadata_cap(init, ctx);
    cap
}

/// Put twin of `register_call`.
public(package) fun register_put<U, S, D0, D1, D2, D3, D4, D5, D6, D7, D8, D9>(
    registry: &mut CoinRegistry,
    expiry_minutes: u32,
    sig: u64,
    exp: u8,
    decimals: u8,
    ctx: &mut TxContext,
): TreasuryCap<OptionPut<U, S, D0, D1, D2, D3, D4, D5, D6, D7, D8, D9>> {
    let actual = type_name::with_defining_ids<
        OptionPut<U, S, D0, D1, D2, D3, D4, D5, D6, D7, D8, D9>,
    >().into_string().into_bytes();
    assert!(
        actual == expected_type_bytes<U, S>(b"OptionPut", expiry_minutes, sig, exp),
        errors::encoding_mismatch(),
    );
    let (init, cap) = coin_registry::new_currency<
        OptionPut<U, S, D0, D1, D2, D3, D4, D5, D6, D7, D8, D9>,
    >(
        registry,
        decimals,
        b"oPUT".to_string(),
        option_name(b"SuiOptions Put ", sig, exp, expiry_minutes),
        b"SuiOptions tokenized cash-secured put (runtime currency)".to_string(),
        b"".to_string(),
        ctx,
    );
    coin_registry::finalize_and_delete_metadata_cap(init, ctx);
    cap
}

// ═══════════════════ expected-type-string construction ═══════════════════

/// `type_name::with_defining_ids` renders generics as
/// `<addr>::<module>::<Name><arg,arg,…>` — bare 64-hex addresses, comma
/// separator, no spaces (verified against the pinned framework by
/// `any_strike_tests::encoding_matches_type_name`). We rebuild that exact
/// string from the numeric arguments and demand equality.
fun expected_type_bytes<U, S>(
    struct_name: vector<u8>,
    expiry_minutes: u32,
    sig: u64,
    exp: u8,
): vector<u8> {
    // "<addr>::enc0::B" / "<addr>::enc1::B" — from a sample marker in each
    // module, minus its two hex chars.
    let mut prefix0 = type_name::with_defining_ids<enc0::B00>().into_string().into_bytes();
    prefix0.pop_back();
    prefix0.pop_back();
    let mut prefix1 = type_name::with_defining_ids<enc1::B80>().into_string().into_bytes();
    prefix1.pop_back();
    prefix1.pop_back();
    // "<addr>::option_coin::" — from this module's own root marker-free
    // neighbour: derive from prefix0 by swapping the module segment? No —
    // just take it from the OptionCall-free helper below.
    let mut out = module_prefix();
    out.append(struct_name);
    out.push_back(60); // '<'
    out.append(type_name::with_defining_ids<U>().into_string().into_bytes());
    out.push_back(44); // ','
    out.append(type_name::with_defining_ids<S>().into_string().into_bytes());

    // The 10-byte stream: minutes u32 ‖ sig u40 ‖ exp u8, MSB first.
    let mut bytes = vector<u8>[];
    push_be(&mut bytes, expiry_minutes as u64, 4);
    push_be(&mut bytes, sig, 5);
    bytes.push_back(exp);

    let hex = b"0123456789ABCDEF";
    let mut i = 0;
    while (i < 10) {
        let byte = bytes[i];
        out.push_back(44); // ','
        if (byte < 128) { out.append(prefix0) } else { out.append(prefix1) };
        out.push_back(hex[((byte >> 4) as u64)]);
        out.push_back(hex[((byte & 0xF) as u64)]);
        i = i + 1;
    };
    out.push_back(62); // '>'
    out
}

/// "<addr>::option_coin::" — this module's own type prefix, taken from a
/// private helper struct's type name minus its name.
public struct Anchor has drop {}

fun module_prefix(): vector<u8> {
    let mut p = type_name::with_defining_ids<Anchor>().into_string().into_bytes();
    // strip "Anchor" (6 chars)
    let mut i = 0;
    while (i < 6) { p.pop_back(); i = i + 1; };
    p
}

/// Push `count` big-endian bytes of `value`.
fun push_be(out: &mut vector<u8>, value: u64, count: u8) {
    let mut i = count;
    while (i > 0) {
        i = i - 1;
        out.push_back(((value >> (8 * (i as u8))) & 0xFF) as u8);
    };
}

/// "SuiOptions Call <sig>[e-<exp>] exp <minutes>m" — protocol-authored,
/// human-legible, and immutable once the metadata cap is deleted.
fun option_name(kind: vector<u8>, sig: u64, exp: u8, expiry_minutes: u32): String {
    let mut n = kind.to_string();
    n.append(std::u64::to_string(sig));
    if (exp > 0) {
        n.append(b"e-".to_string());
        n.append(std::u64::to_string(exp as u64));
    };
    n.append(b" exp ".to_string());
    n.append(std::u64::to_string(expiry_minutes as u64));
    n.append(b"m".to_string());
    n
}

#[test_only]
public fun expected_type_bytes_for_testing<U, S>(
    struct_name: vector<u8>,
    expiry_minutes: u32,
    sig: u64,
    exp: u8,
): vector<u8> {
    expected_type_bytes<U, S>(struct_name, expiry_minutes, sig, exp)
}

#[test_only]
public fun normalize_strike_for_testing(strike: u128, strike_scale: u8): (u64, u8) {
    normalize_strike(strike, strike_scale)
}
