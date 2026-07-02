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
    let a = message::new(2, 1, 7, filled(0xab, 32), filled(0xcd, 32), b"x");
    let b = message::new(2, 1, 7, filled(0xab, 32), filled(0xcd, 32), b"x");
    assert!(message::hash(&a) == message::hash(&b), 0);

    // Any field change perturbs the digest.
    let diff_nonce = message::new(2, 1, 8, filled(0xab, 32), filled(0xcd, 32), b"x");
    let diff_payload = message::new(2, 1, 7, filled(0xab, 32), filled(0xcd, 32), b"y");
    assert!(message::hash(&a) != message::hash(&diff_nonce), 1);
    assert!(message::hash(&a) != message::hash(&diff_payload), 2);
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
    let expected = x"7b767c416104fbef99880be0416fa07353493afb6547ad67d700029ce09572af";
    assert!(message::hash(&m) == expected, 0);
}

#[test]
#[expected_failure]
fun rejects_non_bytes32_src_app() {
    let _ = message::new(2, 1, 7, filled(0xab, 31), filled(0xcd, 32), b"x");
}
