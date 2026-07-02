//! Signature envelope carried alongside a delivered message (bridge-spec.md
//! §2.3). Mirrors `sui_bridge::envelope` and `Envelope.sol`: the Inbox selects
//! the verifier by `scheme_tag` and looks the group key up by `group_pubkey_id`.

use serde::{Deserialize, Serialize};

pub const SCHEME_ED25519: u8 = 0;
pub const SCHEME_ECDSA_SECP256K1: u8 = 1;

/// Signature scheme tag. Ed25519 (FROST) is used for Sui-destined messages,
/// ECDSA/secp256k1 (GG20) for EVM-destined ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Ed25519,
    EcdsaSecp256k1,
}

impl Scheme {
    pub fn tag(self) -> u8 {
        match self {
            Scheme::Ed25519 => SCHEME_ED25519,
            Scheme::EcdsaSecp256k1 => SCHEME_ECDSA_SECP256K1,
        }
    }

    pub fn from_tag(tag: u8) -> Option<Scheme> {
        match tag {
            SCHEME_ED25519 => Some(Scheme::Ed25519),
            SCHEME_ECDSA_SECP256K1 => Some(Scheme::EcdsaSecp256k1),
            _ => None,
        }
    }

    /// The scheme a destination chain family verifies with.
    pub fn for_family(family: u8) -> Option<Scheme> {
        match family {
            crate::chain_id::FAMILY_EVM => Some(Scheme::EcdsaSecp256k1),
            crate::chain_id::FAMILY_SUI => Some(Scheme::Ed25519),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureEnvelope {
    pub scheme_tag: u8,
    pub group_pubkey_id: u32,
    #[serde(with = "crate::message::hex_vec")]
    pub signature: Vec<u8>,
}

impl SignatureEnvelope {
    pub fn new(scheme: Scheme, group_pubkey_id: u32, signature: Vec<u8>) -> Self {
        Self { scheme_tag: scheme.tag(), group_pubkey_id, signature }
    }

    /// Standard **BCS** serialization matching `sui_bridge::envelope::from_bcs`
    /// (scheme_tag, group_pubkey_id, signature) — the plain `vector<u8>` arg a
    /// relayer passes to the Sui `bridge_receive`.
    pub fn to_move_bcs(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(self.scheme_tag);
        out.extend_from_slice(&self.group_pubkey_id.to_le_bytes());
        crate::message::push_uleb128(&mut out, self.signature.len() as u64);
        out.extend_from_slice(&self.signature);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain_id;

    #[test]
    fn scheme_tag_round_trip_and_family_mapping() {
        assert_eq!(Scheme::from_tag(Scheme::Ed25519.tag()), Some(Scheme::Ed25519));
        assert_eq!(Scheme::from_tag(Scheme::EcdsaSecp256k1.tag()), Some(Scheme::EcdsaSecp256k1));
        assert_eq!(Scheme::from_tag(9), None);
        assert_eq!(Scheme::for_family(chain_id::FAMILY_EVM), Some(Scheme::EcdsaSecp256k1));
        assert_eq!(Scheme::for_family(chain_id::FAMILY_SUI), Some(Scheme::Ed25519));
    }

    /// BCS layout: scheme_tag (u8), group_pubkey_id (u32 LE), signature
    /// (ULEB128 len + bytes) — what `sui_bridge::envelope::from_bcs` decodes.
    #[test]
    fn to_move_bcs_layout() {
        let e = SignatureEnvelope { scheme_tag: 0, group_pubkey_id: 1, signature: vec![0xaa, 0xbb] };
        assert_eq!(hex::encode(e.to_move_bcs()), "000100000002aabb");
    }
}
