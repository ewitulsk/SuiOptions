//! Serde adapters for the WS wire format — byte-for-byte the
//! solana-quoting-service `coding` module (the bot is the counterparty on
//! the same socket, so the encodings must match exactly).
//!
//! - Integers (u64) ride as decimal strings in JSON (avoids JS precision
//!   loss past 2^53).
//! - `Vec<u8>` (signatures, challenges, raw pubkeys) rides as
//!   `0x`-prefixed hex, exactly like the Sui twin. Pubkey *identities*
//!   (accounts, buckets, mints) are base58 strings and use plain `String`
//!   fields instead.

pub mod u64_string {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &u64, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&v.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<u64, D::Error> {
        let s = String::deserialize(de)?;
        s.parse::<u64>().map_err(serde::de::Error::custom)
    }
}

pub mod bytes_hex {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Vec<u8>, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&format!("0x{}", hex::encode(v)))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(de)?;
        let stripped = s.strip_prefix("0x").unwrap_or(&s);
        hex::decode(stripped).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
    struct U64Wrap(#[serde(with = "super::u64_string")] u64);

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
    fn bytes_json_is_hex_prefixed_and_accepts_unprefixed() {
        let j = serde_json::to_string(&Bytes(vec![0xde, 0xad, 0xbe, 0xef])).unwrap();
        assert_eq!(j, "\"0xdeadbeef\"");
        let back: Bytes = serde_json::from_str("\"deadbeef\"").unwrap();
        assert_eq!(back, Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
    }
}
