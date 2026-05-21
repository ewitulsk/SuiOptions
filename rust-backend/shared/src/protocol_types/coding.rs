//! Serde adapters that switch on `is_human_readable`.
//!
//! - Integers (u64 / u128) ride as decimal strings in JSON (§4.3 — avoids JS
//!   precision loss past 2^53) and as native little-endian in BCS.
//! - `Vec<u8>` rides as `0x`-prefixed hex in JSON and as ULEB128-length-prefixed
//!   raw bytes in BCS (which matches Move `vector<u8>` byte-for-byte).

pub mod u64_string {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &u64, ser: S) -> Result<S::Ok, S::Error> {
        if ser.is_human_readable() {
            ser.serialize_str(&v.to_string())
        } else {
            ser.serialize_u64(*v)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<u64, D::Error> {
        if de.is_human_readable() {
            let s = String::deserialize(de)?;
            s.parse::<u64>().map_err(serde::de::Error::custom)
        } else {
            u64::deserialize(de)
        }
    }
}

pub mod u128_string {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &u128, ser: S) -> Result<S::Ok, S::Error> {
        if ser.is_human_readable() {
            ser.serialize_str(&v.to_string())
        } else {
            ser.serialize_u128(*v)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<u128, D::Error> {
        if de.is_human_readable() {
            let s = String::deserialize(de)?;
            s.parse::<u128>().map_err(serde::de::Error::custom)
        } else {
            u128::deserialize(de)
        }
    }
}

pub mod bytes_hex {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &Vec<u8>, ser: S) -> Result<S::Ok, S::Error> {
        if ser.is_human_readable() {
            ser.serialize_str(&format!("0x{}", hex::encode(v)))
        } else {
            v.serialize(ser)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        if de.is_human_readable() {
            let s = String::deserialize(de)?;
            let stripped = s.strip_prefix("0x").unwrap_or(&s);
            hex::decode(stripped).map_err(serde::de::Error::custom)
        } else {
            Vec::<u8>::deserialize(de)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
    struct U64Wrap(#[serde(with = "super::u64_string")] u64);

    #[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
    struct U128Wrap(#[serde(with = "super::u128_string")] u128);

    #[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
    struct Bytes(#[serde(with = "super::bytes_hex")] Vec<u8>);

    #[test]
    fn u64_json_is_decimal_string() {
        let j = serde_json::to_string(&U64Wrap(1_748_534_400_000)).unwrap();
        assert_eq!(j, "\"1748534400000\"");
        let back: U64Wrap = serde_json::from_str(&j).unwrap();
        assert_eq!(back, U64Wrap(1_748_534_400_000));
    }

    #[test]
    fn u64_bcs_is_little_endian() {
        let bytes = bcs::to_bytes(&U64Wrap(0x0102_0304_0506_0708)).unwrap();
        assert_eq!(bytes, vec![0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn u128_json_handles_large_values() {
        let big = u128::MAX - 17;
        let j = serde_json::to_string(&U128Wrap(big)).unwrap();
        let back: U128Wrap = serde_json::from_str(&j).unwrap();
        assert_eq!(back, U128Wrap(big));
    }

    #[test]
    fn bytes_json_is_hex_prefixed() {
        let j = serde_json::to_string(&Bytes(vec![0xde, 0xad, 0xbe, 0xef])).unwrap();
        assert_eq!(j, "\"0xdeadbeef\"");
        let back: Bytes = serde_json::from_str(&j).unwrap();
        assert_eq!(back, Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
    }

    #[test]
    fn bytes_json_accepts_unprefixed_hex() {
        let back: Bytes = serde_json::from_str("\"deadbeef\"").unwrap();
        assert_eq!(back, Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
    }

    #[test]
    fn bytes_bcs_is_uleb_then_raw() {
        // 3 bytes payload → ULEB128(3) = 0x03, then the bytes.
        let bytes = bcs::to_bytes(&Bytes(vec![0x11, 0x22, 0x33])).unwrap();
        assert_eq!(bytes, vec![0x03, 0x11, 0x22, 0x33]);
    }
}
