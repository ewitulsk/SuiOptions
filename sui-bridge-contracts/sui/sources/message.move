/// Canonical, chain-neutral cross-chain message and its keccak256 digest.
///
/// The digest MUST be byte-identical on every chain so the same threshold
/// signature verifies everywhere (bridge-spec.md §2.2). BCS is *not* used for
/// the wire format because it is little-endian + ULEB128 length-prefixed, which
/// an EVM `abi.encodePacked` cannot reproduce. Instead we fix an explicit
/// big-endian packed layout that both Move and Solidity can emit identically:
///
/// ```
/// version       u8                 1 byte
/// src_chain_id  u32  big-endian    4 bytes
/// dst_chain_id  u32  big-endian    4 bytes
/// nonce         u64  big-endian    8 bytes
/// src_app       bytes32           32 bytes
/// dst_app       bytes32           32 bytes
/// payload_len   u32  big-endian    4 bytes   (guards against payload ambiguity)
/// payload       bytes        payload_len bytes
/// ```
///
/// `message_hash = keccak256(encode(message))`. Signers sign over this 32-byte
/// digest (Ed25519 verify is performed over the digest bytes on Sui; ecrecover
/// over the same digest on EVM), keeping "the signed digest is identical
/// everywhere" literally true.
module sui_bridge::message;

use sui::hash;
use sui_bridge::errors;

/// Current canonical format version. Start at 1 (bridge-spec.md §2.2).
const VERSION: u8 = 1;

const BYTES32_LEN: u64 = 32;

public struct CrossChainMessage has copy, drop, store {
    version: u8,
    src_chain_id: u32,
    dst_chain_id: u32,
    nonce: u64,
    /// 32-byte sender app address (Sui object/package ID, or left-padded EVM
    /// address). Length is enforced to be exactly 32.
    src_app: vector<u8>,
    /// 32-byte recipient app address (same encoding as `src_app`).
    dst_app: vector<u8>,
    /// Opaque to Layer 1; defined by the destination app.
    payload: vector<u8>,
}

public fun version_byte(): u8 { VERSION }

/// Construct a message with the current format version, validating the
/// bytes32 address fields.
public fun new(
    src_chain_id: u32,
    dst_chain_id: u32,
    nonce: u64,
    src_app: vector<u8>,
    dst_app: vector<u8>,
    payload: vector<u8>,
): CrossChainMessage {
    assert!(src_app.length() == BYTES32_LEN, errors::bad_bytes32_length());
    assert!(dst_app.length() == BYTES32_LEN, errors::bad_bytes32_length());
    CrossChainMessage {
        version: VERSION,
        src_chain_id,
        dst_chain_id,
        nonce,
        src_app,
        dst_app,
        payload,
    }
}

// --- field accessors ---
public fun version(m: &CrossChainMessage): u8 { m.version }
public fun src_chain_id(m: &CrossChainMessage): u32 { m.src_chain_id }
public fun dst_chain_id(m: &CrossChainMessage): u32 { m.dst_chain_id }
public fun nonce(m: &CrossChainMessage): u64 { m.nonce }
public fun src_app(m: &CrossChainMessage): vector<u8> { m.src_app }
public fun dst_app(m: &CrossChainMessage): vector<u8> { m.dst_app }
public fun payload(m: &CrossChainMessage): vector<u8> { m.payload }

/// Canonical big-endian packed serialization (see module doc).
public fun encode(m: &CrossChainMessage): vector<u8> {
    assert!(m.version == VERSION, errors::unsupported_version());
    assert!(m.src_app.length() == BYTES32_LEN, errors::bad_bytes32_length());
    assert!(m.dst_app.length() == BYTES32_LEN, errors::bad_bytes32_length());

    let mut out = vector<u8>[];
    out.push_back(m.version);
    append_u32_be(&mut out, m.src_chain_id);
    append_u32_be(&mut out, m.dst_chain_id);
    append_u64_be(&mut out, m.nonce);
    out.append(m.src_app);
    out.append(m.dst_app);
    append_u32_be(&mut out, m.payload.length() as u32);
    out.append(m.payload);
    out
}

/// `keccak256(encode(message))` — the 32-byte digest signers sign over.
public fun hash(m: &CrossChainMessage): vector<u8> {
    hash::keccak256(&encode(m))
}

// --- big-endian integer encoders ---

public fun append_u32_be(out: &mut vector<u8>, v: u32) {
    let mut i = 4u8;
    while (i > 0) {
        i = i - 1;
        out.push_back(((v >> (8 * (i as u8))) & 0xff) as u8);
    };
}

public fun append_u64_be(out: &mut vector<u8>, v: u64) {
    let mut i = 8u8;
    while (i > 0) {
        i = i - 1;
        out.push_back(((v >> (8 * (i as u8))) & 0xff) as u8);
    };
}
