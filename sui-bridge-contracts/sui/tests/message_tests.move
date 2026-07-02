#[test_only]
module sui_bridge::message_tests;

use sui_bridge::chain_id;
use sui_bridge::message;

fun filled(b: u8, n: u64): vector<u8> {
    let mut v = vector<u8>[];
    let mut i = 0;
    while (i < n) { v.push_back(b); i = i + 1; };
    v
}

#[test]
fun encode_length_is_fixed_header_plus_payload() {
    let payload = b"hello-bridge";
    let m = message::new(2, 1, 7, filled(0xab, 32), filled(0xcd, 32), payload);
    let enc = message::encode(&m);
    // 1 (version) + 4 + 4 + 8 (ints) + 32 + 32 (apps) + 4 (len) + payload.
    assert!(enc.length() == 1 + 4 + 4 + 8 + 32 + 32 + 4 + payload.length(), 0);
}

#[test]
fun hash_is_deterministic_and_field_sensitive() {
    let ds = message::derive_domain_sep(filled(0x01, 32));
    let a = message::new(2, 1, 7, filled(0xab, 32), filled(0xcd, 32), b"x");
    let b = message::new(2, 1, 7, filled(0xab, 32), filled(0xcd, 32), b"x");
    assert!(message::hash(&a, ds) == message::hash(&b, ds), 0);

    // Any field change perturbs the digest.
    let diff_nonce = message::new(2, 1, 8, filled(0xab, 32), filled(0xcd, 32), b"x");
    let diff_payload = message::new(2, 1, 7, filled(0xab, 32), filled(0xcd, 32), b"y");
    assert!(message::hash(&a, ds) != message::hash(&diff_nonce, ds), 1);
    assert!(message::hash(&a, ds) != message::hash(&diff_payload, ds), 2);

    // A different deployment salt perturbs the digest (domain separation).
    let ds2 = message::derive_domain_sep(filled(0x02, 32));
    assert!(message::hash(&a, ds) != message::hash(&a, ds2), 3);
}

/// Cross-checks the Move keccak256 + canonical encoding against an independent
/// offline implementation (@noble/hashes keccak_256 over the same big-endian
/// packed layout). If the encoding ever drifts, this digest stops matching and
/// signatures produced off-chain would fail on-chain.
#[test]
fun known_digest_vector() {
    // src = HyperEVM (family 2, chainId 998), dst = Sui (family 1, local 0).
    let src = chain_id::new(chain_id::family_evm(), 998);
    let dst = chain_id::new(chain_id::family_sui(), 0);
    let m = message::new(src, dst, 7, filled(0xab, 32), filled(0xcd, 32), b"hello-bridge");
    // Domain-separated under the shared TEST_SALT = 0x01*32 (see bridge-types
    // message.rs and the cross-language vectors from `group_keys` example).
    let ds = message::derive_domain_sep(filled(0x01, 32));
    let expected = x"535392536947463d04988702a5480f431f34efed3cf557dc12aa434c2decd707";
    assert!(message::hash(&m, ds) == expected, 0);
}

#[test]
#[expected_failure]
fun rejects_non_bytes32_src_app() {
    let _ = message::new(2, 1, 7, filled(0xab, 31), filled(0xcd, 32), b"x");
}

/// The BCS bytes a relayer passes to `bridge_receive` (produced by Rust
/// `CrossChainMessage::to_move_bcs`) decode back to the known-vector message and
/// hash to the same domain-separated digest. Regenerate via the `group_keys`
/// example.
#[test]
fun from_bcs_decodes_to_known_digest() {
    let bcs_bytes =
        x"01e603001000000008070000000000000020abababababababababababababababababababababababababababababababab20cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd0c68656c6c6f2d627269646765";
    let m = message::from_bcs(bcs_bytes);
    assert!(message::src_chain_id(&m) == chain_id::new(chain_id::family_evm(), 998), 0);
    assert!(message::dst_chain_id(&m) == chain_id::new(chain_id::family_sui(), 0), 1);
    assert!(message::nonce(&m) == 7, 2);
    assert!(message::payload(&m) == b"hello-bridge", 3);
    let ds = message::derive_domain_sep(filled(0x01, 32));
    let expected = x"535392536947463d04988702a5480f431f34efed3cf557dc12aa434c2decd707";
    assert!(message::hash(&m, ds) == expected, 4);
}
