/// Signature envelope + per-scheme verification adapter.
///
/// The delivered message carries `(scheme_tag, group_pubkey_id, signature)`
/// (bridge-spec.md §2.3). The Inbox looks the registered group key up by
/// `group_pubkey_id` and verifies by `scheme_tag`. Messages destined for Sui
/// are signed with FROST threshold Schnorr over Ed25519, so the Sui adapter
/// implements the Ed25519 path; other scheme tags abort here (an EVM-destined
/// ECDSA signature is verified on the EVM Inbox, not this one).
///
/// Signers sign over the 32-byte `message::hash` digest, so Ed25519 verify is
/// performed directly over the digest bytes — identical preimage to the EVM
/// ecrecover path.
module sui_bridge::envelope;

use sui::bcs;
use sui::ed25519;
use sui_bridge::errors;
use sui_bridge::registry::{Self, GroupKeyRegistry};

const SCHEME_ED25519: u8 = 0;
const SCHEME_ECDSA_SECP256K1: u8 = 1;

public fun scheme_ed25519(): u8 { SCHEME_ED25519 }
public fun scheme_ecdsa_secp256k1(): u8 { SCHEME_ECDSA_SECP256K1 }

public struct SignatureEnvelope has copy, drop, store {
    scheme_tag: u8,
    group_pubkey_id: u32,
    signature: vector<u8>,
}

public fun new(scheme_tag: u8, group_pubkey_id: u32, signature: vector<u8>): SignatureEnvelope {
    SignatureEnvelope { scheme_tag, group_pubkey_id, signature }
}

/// Decode the standard BCS serialization (scheme_tag, group_pubkey_id,
/// signature) a relayer passes as a plain `vector<u8>` arg. Produced off-chain
/// by `bridge_types::SignatureEnvelope::to_move_bcs`.
public fun from_bcs(bytes: vector<u8>): SignatureEnvelope {
    let mut b = bcs::new(bytes);
    let scheme_tag = b.peel_u8();
    let group_pubkey_id = b.peel_u32();
    let signature = b.peel_vec_u8();
    SignatureEnvelope { scheme_tag, group_pubkey_id, signature }
}

public fun scheme_tag(e: &SignatureEnvelope): u8 { e.scheme_tag }
public fun group_pubkey_id(e: &SignatureEnvelope): u32 { e.group_pubkey_id }
public fun signature(e: &SignatureEnvelope): vector<u8> { e.signature }

/// Verify `envelope` over `message_hash` against the registered group key.
/// Aborts on an unknown key id, a scheme/key mismatch, an unsupported scheme,
/// or an invalid signature.
public fun verify(
    keys: &GroupKeyRegistry,
    envelope: &SignatureEnvelope,
    message_hash: &vector<u8>,
) {
    let (registered_scheme, pubkey) = registry::group_key(keys, envelope.group_pubkey_id);
    assert!(registered_scheme == envelope.scheme_tag, errors::scheme_key_mismatch());

    if (envelope.scheme_tag == SCHEME_ED25519) {
        let ok = ed25519::ed25519_verify(&envelope.signature, &pubkey, message_hash);
        assert!(ok, errors::signature_invalid());
    } else {
        // ECDSA/secp256k1 is the EVM-destined path, verified on the EVM Inbox.
        abort errors::unsupported_scheme()
    }
}
