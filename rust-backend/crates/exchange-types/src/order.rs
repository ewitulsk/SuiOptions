use crate::types::{Digest, ObjectId, SuiAddress, TypeTagStr};
use serde::{Deserialize, Serialize};

/// Mirror of Move `exchange::order::Order`.
///
/// Field order is consensus-critical: BCS encoding depends on it. Never
/// reorder or insert fields without bumping the Move `DOMAIN_VERSION` and the
/// conformance fixtures in `exchange-signing`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Order {
    // -- economic terms --
    pub maker_token: TypeTagStr,
    pub taker_token: TypeTagStr,
    #[serde(with = "u64_as_string")]
    pub maker_amount: u64,
    #[serde(with = "u64_as_string")]
    pub taker_amount: u64,
    #[serde(with = "u64_as_string")]
    pub max_fee_bps: u64,

    // -- parties & permissions --
    pub maker: SuiAddress,
    pub maker_manager_id: ObjectId,
    pub taker: SuiAddress,
    pub sender: SuiAddress,

    // -- validity --
    pub expiry_ms: u64,
    #[serde(with = "u64_as_string")]
    pub salt: u64,
}

impl Order {
    /// Canonical BCS bytes — the exact payload hashed into the order digest
    /// and passed on-chain as `order_bytes`.
    pub fn to_bcs(&self) -> Vec<u8> {
        bcs::to_bytes(self).expect("Order BCS serialization cannot fail")
    }
}

/// Supported maker signature schemes. The discriminant is the Sui address
/// derivation flag and the on-chain scheme prefix byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignatureScheme {
    Ed25519 = 0x00,
    Secp256k1 = 0x01,
}

impl SignatureScheme {
    pub fn flag(self) -> u8 {
        self as u8
    }

    pub fn from_flag(flag: u8) -> Option<Self> {
        match flag {
            0x00 => Some(SignatureScheme::Ed25519),
            0x01 => Some(SignatureScheme::Secp256k1),
            _ => None,
        }
    }
}

/// An order plus everything needed to settle it on-chain: the maker (or
/// delegated signer) signature and public key, and the market registry it is
/// domain-bound to. This is the wire format of `POST /v1/orders` and the
/// "fill ticket" served by `GET /v1/markets/{m}/orders/{digest}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedOrder {
    #[serde(flatten)]
    pub order: Order,
    pub registry_id: ObjectId,
    pub scheme: SignatureScheme,
    /// Raw signature bytes (64), base64 in JSON. On-chain the scheme flag is
    /// prepended: `[flag] || sig`.
    #[serde(with = "base64_bytes")]
    pub signature: Vec<u8>,
    /// 32 bytes for ed25519, 33 (compressed) for secp256k1.
    #[serde(with = "base64_bytes")]
    pub public_key: Vec<u8>,
}

impl SignedOrder {
    /// Scheme-prefixed signature as the Move entry points expect it.
    pub fn prefixed_signature(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(1 + self.signature.len());
        v.push(self.scheme.flag());
        v.extend_from_slice(&self.signature);
        v
    }
}

/// A signed order annotated with its digest (computed at intake and cached —
/// the digest is the primary key everywhere).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedOrder {
    pub signed: SignedOrder,
    pub digest: Digest,
    pub order_bytes: Vec<u8>,
}

mod u64_as_string {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&v.to_string())
        } else {
            s.serialize_u64(*v)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        if d.is_human_readable() {
            #[derive(Deserialize)]
            #[serde(untagged)]
            enum NumOrStr {
                Num(u64),
                Str(String),
            }
            match NumOrStr::deserialize(d)? {
                NumOrStr::Num(n) => Ok(n),
                NumOrStr::Str(s) => s.parse().map_err(serde::de::Error::custom),
            }
        } else {
            u64::deserialize(d)
        }
    }
}

mod base64_bytes {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(s)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_order() -> Order {
        Order {
            maker_token: "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI".into(),
            taker_token: "0x00000000000000000000000000000000000000000000000000000000000000aa::usdc::USDC".into(),
            maker_amount: 50_000_000_000,
            taker_amount: 125_000_000,
            max_fee_bps: 10,
            maker: SuiAddress::parse("0x9f").unwrap(),
            maker_manager_id: SuiAddress::parse("0x71").unwrap(),
            taker: SuiAddress::ZERO,
            sender: SuiAddress::ZERO,
            expiry_ms: 1_754_330_000_000,
            salt: 1_754_329_100_123,
        }
    }

    #[test]
    fn bcs_layout() {
        let o = sample_order();
        let b = o.to_bcs();
        // maker_token: 1-byte ULEB len (78) + 78 utf8 bytes
        assert_eq!(b[0] as usize, o.maker_token.len());
        let back: Order = bcs::from_bytes(&b).unwrap();
        assert_eq!(back, o);
    }

    #[test]
    fn json_roundtrip_camel_case_string_amounts() {
        let o = sample_order();
        let j = serde_json::to_value(&o).unwrap();
        assert_eq!(j["makerAmount"], "50000000000");
        assert!(j["makerToken"].as_str().unwrap().starts_with("0x0000"));
        let back: Order = serde_json::from_value(j).unwrap();
        assert_eq!(back, o);
    }
}
