//! Any-strike option-coin type construction (SO-393 mirror).
//!
//! `options_core::option_coin` manufactures per-bucket coin currencies as
//! generic instantiations `OptionCall<U, S, D0..D9>` where the ten trailing
//! parameters are byte markers spelling the bucket's economics:
//!
//!   bytes 0..4   expiry, minutes since epoch (u32, big-endian)
//!   bytes 4..9   strike significand (u40, big-endian)
//!   byte  9      strike exponent: real strike = sig / 10^exp
//!
//! Markers `B00..B7F` live in module `enc0`, `B80..BFF` in `enc1`. The
//! on-chain registration validates the encoding against the value
//! arguments, so this builder MUST stay byte-compatible with
//! `option_coin::expected_type_bytes` — the localnet E2E
//! (`option-scheduler/tests/anystrike_localnet.rs`) pins the equivalence.

use std::str::FromStr;

use anyhow::{anyhow, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::{StructTag, TypeTag};
use sui_types::base_types::ObjectID;

/// Largest strike significand the 5-byte encoding field can carry (2^40−1).
pub const MAX_SIG: u64 = 0xFF_FFFF_FFFF;

/// Shared system objects the creation PTB references.
pub const CLOCK_OBJECT: &str = "0x6";
pub const COIN_REGISTRY_OBJECT: &str = "0xc";

/// Canonical (significand, exponent) form of a raw (strike, strike_scale)
/// ratio — trailing zeros stripped, mirroring `option_coin::normalize_strike`.
pub fn normalize_strike(strike: u128, strike_scale: u8) -> Result<(u64, u8)> {
    if strike == 0 {
        return Err(anyhow!("strike must be > 0"));
    }
    let mut sig = strike;
    let mut exp = strike_scale;
    while sig % 10 == 0 && exp > 0 {
        sig /= 10;
        exp -= 1;
    }
    if sig > MAX_SIG as u128 {
        return Err(anyhow!(
            "strike significand {sig} exceeds the u40 encoding field (13 significant digits)"
        ));
    }
    Ok((sig as u64, exp))
}

/// Expiry in whole minutes; the encoding (and the on-chain assert) requires
/// minute alignment.
pub fn expiry_minutes(expiry_ms: u64) -> Result<u32> {
    if expiry_ms % 60_000 != 0 {
        return Err(anyhow!("expiry_ms {expiry_ms} is not minute-aligned"));
    }
    u32::try_from(expiry_ms / 60_000).map_err(|_| anyhow!("expiry_ms {expiry_ms} out of range"))
}

/// The ten byte-marker type args for (expiry_minutes, sig, exp).
pub fn marker_type_tags(
    package: ObjectID,
    minutes: u32,
    sig: u64,
    exp: u8,
) -> Result<Vec<TypeTag>> {
    if sig > MAX_SIG {
        return Err(anyhow!("sig {sig} exceeds u40"));
    }
    let mut bytes = minutes.to_be_bytes().to_vec();
    bytes.extend_from_slice(&sig.to_be_bytes()[3..]); // low 5 of 8
    bytes.push(exp);
    bytes
        .into_iter()
        .map(|b| {
            let module = if b < 0x80 { "enc0" } else { "enc1" };
            TypeTag::from_str(&format!("{package}::{module}::B{b:02X}"))
                .map_err(|e| anyhow!("marker tag: {e}"))
        })
        .collect()
}

/// `OptionCall<U, S, D0..D9>` (or `OptionPut<…>`) for the normalized spec.
pub fn option_coin_tag(
    package: ObjectID,
    is_put: bool,
    underlying: &TypeTag,
    settlement: &TypeTag,
    minutes: u32,
    sig: u64,
    exp: u8,
) -> Result<TypeTag> {
    let mut params = vec![underlying.clone(), settlement.clone()];
    params.extend(marker_type_tags(package, minutes, sig, exp)?);
    Ok(TypeTag::Struct(Box::new(StructTag {
        address: package.into(),
        module: Identifier::new("option_coin").expect("static"),
        name: Identifier::new(if is_put { "OptionPut" } else { "OptionCall" }).expect("static"),
        type_params: params,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg() -> ObjectID {
        ObjectID::from_hex_literal("0xabc").unwrap()
    }

    #[test]
    fn normalize_strips_trailing_zeros() {
        assert_eq!(normalize_strike(257100, 4).unwrap(), (2571, 2));
        assert_eq!(normalize_strike(1500, 1).unwrap(), (150, 0));
        assert_eq!(normalize_strike(7, 0).unwrap(), (7, 0));
        assert!(normalize_strike(0, 0).is_err());
        assert!(normalize_strike((MAX_SIG as u128) + 1, 0).is_err());
    }

    #[test]
    fn expiry_requires_minute_alignment() {
        assert_eq!(expiry_minutes(6_000_000_000_000).unwrap(), 100_000_000);
        assert!(expiry_minutes(6_000_000_000_001).is_err());
    }

    #[test]
    fn markers_split_across_enc_modules_by_high_bit() {
        // minutes 50_000_000 = 0x02FAF080, sig 2571 = 0x…0A0B, exp 2.
        let tags = marker_type_tags(pkg(), 50_000_000, 2571, 2).unwrap();
        let names: Vec<String> = tags.iter().map(|t| t.to_canonical_string(false)).collect();
        assert_eq!(names.len(), 10);
        assert!(names[0].ends_with("::enc0::B02"));
        assert!(names[1].ends_with("::enc1::BFA"));
        assert!(names[2].ends_with("::enc1::BF0"));
        assert!(names[3].ends_with("::enc1::B80"));
        assert!(names[4].ends_with("::enc0::B00"));
        assert!(names[7].ends_with("::enc0::B0A"));
        assert!(names[8].ends_with("::enc0::B0B"));
        assert!(names[9].ends_with("::enc0::B02"));
    }

    #[test]
    fn option_coin_tag_is_thirteen_type_nodes() {
        // The 15-node PTB budget rests on this: root + U + S + 10 markers.
        let sui = TypeTag::from_str("0x2::sui::SUI").unwrap();
        let tag = option_coin_tag(pkg(), false, &sui, &sui, 100, 7, 0).unwrap();
        fn nodes(t: &TypeTag) -> usize {
            match t {
                TypeTag::Struct(s) => 1 + s.type_params.iter().map(nodes).sum::<usize>(),
                TypeTag::Vector(v) => 1 + nodes(v),
                _ => 1,
            }
        }
        assert_eq!(nodes(&tag), 13);
        assert!(tag.to_canonical_string(true).contains("::option_coin::OptionCall<"));
    }
}
