//! 32-byte Sui identifiers: `ObjectId` (object/struct UID) and `SuiAddress`
//! (account address). Identical on the wire — distinct in Rust to keep the
//! two from being silently confused at call sites.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// Length of a Sui ID / address in bytes.
pub const SUI_ID_LEN: usize = 32;

macro_rules! define_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub [u8; SUI_ID_LEN]);

        impl $name {
            pub const ZERO: Self = Self([0u8; SUI_ID_LEN]);

            pub fn new(bytes: [u8; SUI_ID_LEN]) -> Self {
                Self(bytes)
            }

            pub fn as_bytes(&self) -> &[u8; SUI_ID_LEN] {
                &self.0
            }

            pub fn to_hex(&self) -> String {
                format!("0x{}", hex::encode(self.0))
            }

            pub fn from_hex(s: &str) -> Result<Self, IdParseError> {
                let stripped = s.strip_prefix("0x").unwrap_or(s);
                let bytes = hex::decode(stripped).map_err(|_| IdParseError::NotHex)?;
                if bytes.len() != SUI_ID_LEN {
                    return Err(IdParseError::WrongLength {
                        got: bytes.len(),
                        want: SUI_ID_LEN,
                    });
                }
                let mut out = [0u8; SUI_ID_LEN];
                out.copy_from_slice(&bytes);
                Ok(Self(out))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.to_hex())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.to_hex())
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::from_hex(s)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
                if ser.is_human_readable() {
                    ser.serialize_str(&self.to_hex())
                } else {
                    self.0.serialize(ser)
                }
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
                if de.is_human_readable() {
                    let s: String = String::deserialize(de)?;
                    Self::from_hex(&s).map_err(serde::de::Error::custom)
                } else {
                    let bytes = <[u8; SUI_ID_LEN]>::deserialize(de)?;
                    Ok(Self(bytes))
                }
            }
        }
    };
}

define_id!(ObjectId, "Sui object ID (32 bytes).");
define_id!(SuiAddress, "Sui account address (32 bytes).");

#[derive(Debug, thiserror::Error)]
pub enum IdParseError {
    #[error("identifier is not valid hex")]
    NotHex,
    #[error("identifier has wrong length: got {got}, want {want}")]
    WrongLength { got: usize, want: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = i as u8;
        }
        let id = ObjectId::new(bytes);
        let s = id.to_hex();
        assert_eq!(s.len(), 2 + 64);
        assert_eq!(ObjectId::from_hex(&s).unwrap(), id);
    }

    #[test]
    fn json_is_hex_string() {
        let id = ObjectId::new([0xab; 32]);
        let j = serde_json::to_string(&id).unwrap();
        assert_eq!(j, format!("\"0x{}\"", "ab".repeat(32)));
        let back: ObjectId = serde_json::from_str(&j).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn bcs_is_raw_32_bytes() {
        let id = ObjectId::new([0xcd; 32]);
        let bytes = bcs::to_bytes(&id).unwrap();
        assert_eq!(bytes, vec![0xcd; 32]);
    }

    #[test]
    fn object_and_address_distinct_at_type_level() {
        // Just compile-time: types are not interchangeable.
        fn takes_id(_: ObjectId) {}
        fn takes_addr(_: SuiAddress) {}
        takes_id(ObjectId::ZERO);
        takes_addr(SuiAddress::ZERO);
    }
}
