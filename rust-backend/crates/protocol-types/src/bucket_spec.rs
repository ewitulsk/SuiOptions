//! A bucket's full economic identity — the off-chain mirror of
//! `options_core::bucket_registry::BucketKey`.
//!
//! This is what a quote binds (see [`crate::quote::Quote`]) and what the RFQ
//! wire carries in place of a bucket id. Buckets are created just-in-time, so
//! by the time an MM prices a strike its bucket may not exist yet; the spec
//! names the economics, and `bucket_registry` guarantees one bucket per spec.
//!
//! # The `TypeName` encoding trap
//!
//! `asset` and `settlement` hold **chain-form** type strings — the canonical
//! path with the address zero-padded to 64 hex chars and *no* `0x` prefix,
//! exactly what Move's `type_name::with_defining_ids` produces. That is NOT
//! the `0x`-prefixed form our services emit to clients. BCS-encoding the
//! client-facing form yields different bytes and every signature fails
//! verification, so construct specs through [`BucketSpec::new`] rather than
//! assembling the struct literal from whatever string is at hand.
//!
//! BCS layout matches the Move struct field-for-field: a Move `TypeName`
//! wraps an `ascii::String` wraps a `vector<u8>`, and BCS flattens nested
//! structs, so it encodes identically to a Rust `String` (ULEB128 length +
//! bytes).

use serde::{Deserialize, Serialize};

use crate::asset::chain_form_move_type;
use crate::coding::u64_string;

/// Largest strike significand the u40 encoding field carries (2^40 − 1).
/// Mirrors `sui_tx::tx::option_coin::MAX_SIG` and the on-chain assert.
pub const MAX_SIG: u64 = 0xFF_FFFF_FFFF;

/// Milliseconds per minute — expiries must be minute-aligned for the
/// option-coin type encoding to stay injective.
const MINUTE_MS: u64 = 60_000;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BucketSpec {
    /// Underlying coin type, CHAIN FORM (no `0x`). See the module docs.
    pub asset: String,
    /// Settlement coin type, CHAIN FORM (no `0x`).
    pub settlement: String,
    #[serde(with = "u64_string")]
    pub expiry_ms: u64,
    /// Normalized strike significand: real ratio is `sig / 10^exp`.
    #[serde(with = "u64_string")]
    pub sig: u64,
    /// Normalized strike exponent.
    pub exp: u8,
    pub is_put: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SpecError {
    #[error("strike must be greater than zero")]
    ZeroStrike,
    #[error("strike significand exceeds the u40 encoding field (13 significant digits)")]
    StrikeNotRepresentable,
    #[error("expiry_ms is not minute-aligned")]
    ExpiryNotAligned,
    #[error("expiry_ms out of range for the u32-minutes encoding")]
    ExpiryOutOfRange,
}

impl BucketSpec {
    /// Build a spec from a raw `(strike, strike_scale)` pair and coin types in
    /// any form. Normalizes the strike and converts both types to chain form,
    /// so this is the only constructor that should be used on a signing path.
    pub fn new(
        asset: &str,
        settlement: &str,
        expiry_ms: u64,
        strike: u128,
        strike_scale: u8,
        is_put: bool,
    ) -> Result<Self, SpecError> {
        let (sig, exp) = normalize_strike(strike, strike_scale)?;
        Ok(Self {
            asset: chain_form_move_type(asset),
            settlement: chain_form_move_type(settlement),
            expiry_ms,
            sig,
            exp,
            is_put,
        })
    }

    /// Real strike ratio (settlement smallest-units per underlying
    /// smallest-unit) as an `f64`, for pricing.
    pub fn strike_scaled(&self) -> f64 {
        self.sig as f64 / 10f64.powi(self.exp as i32)
    }

    /// Expiry in whole minutes — the unit the option-coin type encodes.
    pub fn expiry_minutes(&self) -> Result<u32, SpecError> {
        if self.expiry_ms % MINUTE_MS != 0 {
            return Err(SpecError::ExpiryNotAligned);
        }
        u32::try_from(self.expiry_ms / MINUTE_MS).map_err(|_| SpecError::ExpiryOutOfRange)
    }

    /// True when this spec can actually be created on chain. A spec that
    /// fails this is quotable in principle but its bucket could never exist,
    /// so callers should refuse it up front rather than at execution.
    pub fn is_creatable(&self) -> bool {
        self.expiry_minutes().is_ok() && self.sig > 0 && self.sig <= MAX_SIG
    }
}

/// Canonical `(significand, exponent)` for a raw `(strike, strike_scale)`:
/// trailing zeros stripped. Mirrors `option_coin::normalize_strike`, so two
/// raw encodings of one economic strike collapse to a single spec — and to a
/// single bucket.
pub fn normalize_strike(strike: u128, strike_scale: u8) -> Result<(u64, u8), SpecError> {
    if strike == 0 {
        return Err(SpecError::ZeroStrike);
    }
    let (mut sig, mut exp) = (strike, strike_scale);
    while sig % 10 == 0 && exp > 0 {
        sig /= 10;
        exp -= 1;
    }
    if sig > MAX_SIG as u128 {
        return Err(SpecError::StrikeNotRepresentable);
    }
    Ok((sig as u64, exp))
}

/// The spec's option-coin type literal, `0x`-prefixed for client and PTB use.
///
/// A pure function of the spec — valid whether or not the bucket exists —
/// because `create_*_any_strike` pins the coin type to
/// `(U, S, expiry, sig, exp)` through the marker-encoding assert. The ten
/// trailing type args are byte markers: expiry minutes (u32 BE) ‖ significand
/// (u40 BE) ‖ exponent (u8), with `B00..B7F` in module `enc0` and `B80..BFF`
/// in `enc1`.
///
/// Byte-compatible with `sui_tx::tx::option_coin`, the frontend's
/// `optionCoinTypeFor`, and the on-chain builder.
pub fn option_coin_type(package: &str, spec: &BucketSpec) -> Result<String, SpecError> {
    let minutes = spec.expiry_minutes()?;
    if spec.sig > MAX_SIG {
        return Err(SpecError::StrikeNotRepresentable);
    }
    let pkg = crate::asset::canonicalize_move_type(&format!("{package}::x::X"))
        .split_once("::")
        .map(|(a, _)| a.to_string())
        .unwrap_or_else(|| package.to_string());

    let mut bytes = minutes.to_be_bytes().to_vec();
    bytes.extend_from_slice(&spec.sig.to_be_bytes()[3..]); // low 5 of 8
    bytes.push(spec.exp);
    let markers: Vec<String> = bytes
        .iter()
        .map(|b| {
            let module = if *b < 0x80 { "enc0" } else { "enc1" };
            format!("{pkg}::{module}::B{b:02X}")
        })
        .collect();

    let root = if spec.is_put { "OptionPut" } else { "OptionCall" };
    Ok(format!(
        "{pkg}::option_coin::{root}<{},{},{}>",
        crate::asset::canonicalize_move_type(&spec.asset),
        crate::asset::canonicalize_move_type(&spec.settlement),
        markers.join(",")
    ))
}

/// Recover the spec from an option-coin type string — the inverse of
/// [`option_coin_type`].
///
/// The encoding is injective, so a `Coin<OptionCall<..>>` balance is enough to
/// know exactly which bucket it belongs to. That matters for anything that has
/// to enumerate holdings: with permissionless creation there is no bounded
/// catalog to scan, but a wallet or vault balance names its own bucket.
///
/// Returns `None` for anything that is not an option coin of `package`.
pub fn decode_option_coin_type(package: &str, coin_type: &str) -> Option<BucketSpec> {
    let pkg = crate::asset::canonicalize_move_type(&format!("{package}::x::X"))
        .split_once("::")
        .map(|(a, _)| a.to_string())?;
    let t = crate::asset::canonicalize_move_type(coin_type);

    let (root, args) = t.strip_suffix('>')?.split_once('<')?;
    let is_put = match root.strip_prefix(&format!("{pkg}::option_coin::"))? {
        "OptionCall" => false,
        "OptionPut" => true,
        _ => return None,
    };

    // The type args are flat here — the markers are nullary and U/S are
    // ordinary struct tags — but U or S could themselves be generic, so split
    // at depth zero rather than on every comma.
    let mut parts: Vec<String> = Vec::new();
    let (mut depth, mut cur) = (0usize, String::new());
    for c in args.chars() {
        match c {
            '<' => { depth += 1; cur.push(c); }
            '>' => { depth -= 1; cur.push(c); }
            ',' if depth == 0 => parts.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    parts.push(cur);
    if parts.len() != 12 {
        return None;
    }

    let mut bytes = [0u8; 10];
    for (i, marker) in parts[2..].iter().enumerate() {
        let name = marker.rsplit("::").next()?;
        let hex = name.strip_prefix('B')?;
        bytes[i] = u8::from_str_radix(hex, 16).ok()?;
    }
    let minutes = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let sig = u64::from_be_bytes([0, 0, 0, bytes[4], bytes[5], bytes[6], bytes[7], bytes[8]]);
    let exp = bytes[9];

    Some(BucketSpec {
        asset: chain_form_move_type(&parts[0]),
        settlement: chain_form_move_type(&parts[1]),
        expiry_ms: minutes as u64 * MINUTE_MS,
        sig,
        exp,
        is_put,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> BucketSpec {
        BucketSpec::new("0x9::tbtc::TBTC", "0x9::tusdc::TUSDC", 6_000_000_000_000, 2571, 2, false)
            .unwrap()
    }

    #[test]
    fn normalize_strips_trailing_zeros() {
        assert_eq!(normalize_strike(257100, 4).unwrap(), (2571, 2));
        assert_eq!(normalize_strike(1500, 1).unwrap(), (150, 0));
        assert_eq!(normalize_strike(7, 0).unwrap(), (7, 0));
        assert_eq!(normalize_strike(0, 0), Err(SpecError::ZeroStrike));
        assert_eq!(
            normalize_strike((MAX_SIG as u128) + 1, 0),
            Err(SpecError::StrikeNotRepresentable)
        );
    }

    #[test]
    fn equivalent_raw_strikes_produce_one_spec() {
        let a = BucketSpec::new("0x9::a::A", "0x9::b::B", 60_000, 50_000, 0, false).unwrap();
        let b = BucketSpec::new("0x9::a::A", "0x9::b::B", 60_000, 500_000, 1, false).unwrap();
        assert_eq!(a, b);
    }

    /// The signing-layout trap: the spec must carry chain-form types, so a
    /// `0x`-prefixed input and a bare one must produce identical BCS bytes.
    #[test]
    fn types_are_stored_in_chain_form() {
        let s = spec();
        assert!(!s.asset.starts_with("0x"), "asset must be chain form, got {}", s.asset);
        assert!(s.asset.starts_with("0000"), "address must be 64-padded, got {}", s.asset);

        let bare =
            BucketSpec::new(&s.asset, &s.settlement, s.expiry_ms, 2571, 2, false).unwrap();
        assert_eq!(bcs::to_bytes(&s).unwrap(), bcs::to_bytes(&bare).unwrap());
    }

    /// A Move `TypeName` BCS-encodes as ULEB128 length + ascii bytes, exactly
    /// like a Rust `String`. Pin the leading bytes so a serde attribute that
    /// changed the representation would fail loudly here.
    #[test]
    fn bcs_layout_starts_with_the_asset_string() {
        let s = spec();
        let bytes = bcs::to_bytes(&s).unwrap();
        let mut expect = vec![s.asset.len() as u8];
        expect.extend_from_slice(s.asset.as_bytes());
        assert!(bytes.starts_with(&expect));
        // …and ends with sig(8) + exp(1) + is_put(1) after expiry(8).
        assert_eq!(bytes[bytes.len() - 1], 0); // is_put = false
        assert_eq!(bytes[bytes.len() - 2], 2); // exp
    }

    #[test]
    fn option_coin_type_matches_the_marker_encoding() {
        // minutes 100_000_000 = 0x05F5E100, sig 2571 = 0x…0A0B, exp 2.
        let s = BucketSpec::new(
            "0x9::a::A",
            "0x9::b::B",
            100_000_000 * MINUTE_MS,
            2571,
            2,
            false,
        )
        .unwrap();
        let t = option_coin_type("0xabc", &s).unwrap();
        assert!(t.contains("::option_coin::OptionCall<"));
        assert!(t.contains("::enc0::B05,"));
        assert!(t.contains("::enc1::BF5,"));
        assert!(t.ends_with("::enc0::B02>"));
        // Root + U + S + 10 markers = 13 type nodes, under the 15-node cap.
        assert_eq!(t.matches("::").count() / 2, 13);
    }

    #[test]
    fn put_root_differs() {
        let mut s = spec();
        s.is_put = true;
        assert!(option_coin_type("0xabc", &s).unwrap().contains("::OptionPut<"));
    }

    /// The decoder is the exact inverse of the encoder, for both kinds — this
    /// is what lets a holdings scan work off a coin balance alone, with no
    /// catalog to consult.
    #[test]
    fn decode_round_trips_the_encoding() {
        for is_put in [false, true] {
            let s = BucketSpec::new(
                "0x9::tbtc::TBTC",
                "0x9::tusdc::TUSDC",
                100_000_000 * MINUTE_MS,
                2571,
                2,
                is_put,
            )
            .unwrap();
            let t = option_coin_type("0xabc", &s).unwrap();
            assert_eq!(decode_option_coin_type("0xabc", &t).as_ref(), Some(&s));
        }
    }

    #[test]
    fn decode_rejects_foreign_and_malformed_types() {
        let s = spec();
        let t = option_coin_type("0xabc", &s).unwrap();
        // Right shape, wrong package.
        assert_eq!(decode_option_coin_type("0xdef", &t), None);
        // Not an option coin at all.
        assert_eq!(decode_option_coin_type("0xabc", "0x2::sui::SUI"), None);
        // Right root, wrong arity.
        assert_eq!(
            decode_option_coin_type("0xabc", "0xabc::option_coin::OptionCall<0x9::a::A>"),
            None
        );
    }

    #[test]
    fn unaligned_expiry_is_not_creatable() {
        let s = BucketSpec::new("0x9::a::A", "0x9::b::B", 60_001, 1, 0, false).unwrap();
        assert!(!s.is_creatable());
        assert_eq!(s.expiry_minutes(), Err(SpecError::ExpiryNotAligned));
    }
}
