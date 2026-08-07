use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// A 32-byte Sui address.
///
/// BCS: 32 raw bytes (matches Move `address`).
/// JSON: `0x`-prefixed 64-hex string.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SuiAddress(pub [u8; 32]);

/// A Sui object ID. Identical wire representation to an address.
pub type ObjectId = SuiAddress;

/// A 32-byte order digest (blake2b-256 of the domain-separated order).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest(pub [u8; 32]);

#[derive(Debug, thiserror::Error)]
#[error("invalid hex value: {0}")]
pub struct ParseHexError(String);

fn parse_hex_32(s: &str, pad: bool) -> Result<[u8; 32], ParseHexError> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    let padded;
    let hex_str = if pad && stripped.len() < 64 {
        padded = format!("{:0>64}", stripped);
        &padded
    } else {
        stripped
    };
    let bytes = hex::decode(hex_str).map_err(|_| ParseHexError(s.to_string()))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| ParseHexError(s.to_string()))?;
    Ok(arr)
}

impl SuiAddress {
    pub const ZERO: SuiAddress = SuiAddress([0u8; 32]);

    /// Parse a hex address, left-padding short forms (`0x2` -> 32 bytes).
    pub fn parse(s: &str) -> Result<Self, ParseHexError> {
        Ok(SuiAddress(parse_hex_32(s, true)?))
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; 32]
    }

    pub fn to_hex(&self) -> String {
        format!("0x{}", hex::encode(self.0))
    }
}

impl Digest {
    pub fn parse(s: &str) -> Result<Self, ParseHexError> {
        Ok(Digest(parse_hex_32(s, false)?))
    }

    pub fn to_hex(&self) -> String {
        format!("0x{}", hex::encode(self.0))
    }
}

impl fmt::Debug for SuiAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl fmt::Display for SuiAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

// BCS is not human-readable: emit the raw fixed 32 bytes so the encoding is
// byte-identical to Move `address`. JSON is human-readable: hex string.
impl Serialize for SuiAddress {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.serialize_str(&self.to_hex())
        } else {
            // serde tuple of fixed length => no length prefix, matches Move.
            use serde::ser::SerializeTuple;
            let mut t = serializer.serialize_tuple(32)?;
            for b in &self.0 {
                t.serialize_element(b)?;
            }
            t.end()
        }
    }
}

impl<'de> Deserialize<'de> for SuiAddress {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            let s = String::deserialize(deserializer)?;
            SuiAddress::parse(&s).map_err(serde::de::Error::custom)
        } else {
            struct V;
            impl<'de> serde::de::Visitor<'de> for V {
                type Value = [u8; 32];
                fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                    f.write_str("32 bytes")
                }
                fn visit_seq<A: serde::de::SeqAccess<'de>>(
                    self,
                    mut seq: A,
                ) -> Result<Self::Value, A::Error> {
                    let mut arr = [0u8; 32];
                    for (i, slot) in arr.iter_mut().enumerate() {
                        *slot = seq
                            .next_element()?
                            .ok_or_else(|| serde::de::Error::invalid_length(i, &self))?;
                    }
                    Ok(arr)
                }
            }
            deserializer.deserialize_tuple(32, V).map(SuiAddress)
        }
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.serialize_str(&self.to_hex())
        } else {
            serializer.serialize_bytes(&self.0)
        }
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Digest::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// A canonicalized Move type string: `0x` + full 64-hex address + `::module::Name`.
pub type TypeTagStr = String;

#[derive(Debug, thiserror::Error)]
#[error("invalid move type string: {0}")]
pub struct ParseTypeError(String);

/// Normalize a Move coin type string to the canonical long form the contracts
/// compare against: `0x` + zero-padded 64-hex address + `::module::Name`.
///
/// Move/coin type strings arrive in two non-byte-equal forms (chain `TypeName`
/// without `0x` vs event type string with `0x`, short vs padded addresses);
/// always compare via this function.
pub fn canonicalize_move_type(s: &str) -> Result<TypeTagStr, ParseTypeError> {
    let mut parts = s.splitn(2, "::");
    let addr = parts.next().ok_or_else(|| ParseTypeError(s.to_string()))?;
    let rest = parts.next().ok_or_else(|| ParseTypeError(s.to_string()))?;
    let addr_hex = addr.strip_prefix("0x").unwrap_or(addr);
    if addr_hex.is_empty()
        || addr_hex.len() > 64
        || !addr_hex.chars().all(|c| c.is_ascii_hexdigit())
    {
        return Err(ParseTypeError(s.to_string()));
    }
    let mut mods = rest.split("::");
    let (module, name) = (mods.next(), mods.next());
    match (module, name, mods.next()) {
        (Some(m), Some(n), None) if !m.is_empty() && !n.is_empty() => {
            Ok(format!("0x{:0>64}::{}::{}", addr_hex.to_lowercase(), m, n))
        }
        _ => Err(ParseTypeError(s.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_bcs_is_32_raw_bytes() {
        let a = SuiAddress::parse("0x2").unwrap();
        let bytes = bcs::to_bytes(&a).unwrap();
        assert_eq!(bytes.len(), 32);
        assert_eq!(bytes[31], 2);
        let back: SuiAddress = bcs::from_bytes(&bytes).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn canonicalize() {
        assert_eq!(
            canonicalize_move_type("0x2::sui::SUI").unwrap(),
            format!("0x{}2::sui::SUI", "0".repeat(63))
        );
        // chain TypeName form (no 0x) canonicalizes to the same string
        assert_eq!(
            canonicalize_move_type(&format!("{}2::sui::SUI", "0".repeat(63))).unwrap(),
            canonicalize_move_type("0x2::sui::SUI").unwrap()
        );
        assert!(canonicalize_move_type("nonsense").is_err());
        assert!(canonicalize_move_type("0x2::sui").is_err());
    }
}
