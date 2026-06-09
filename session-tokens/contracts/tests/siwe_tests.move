#[test_only]
module siws_session::siwe_tests;

use siws_session::siwe;

// Reference vector generated with a real secp256k1 key (priv = 0x01 * 32) over
// the EIP-191 personal_sign of the message below. Mirrored byte-for-byte in
// `sdk/src/siwe.test.ts`. See `sdk/gen-siwe.mjs` for the generator.

const ETH_ADDRESS: vector<u8> = x"1a642f0e3c3af545e7acbd38b07251b3990914f1";
const SIG65: vector<u8> = x"8eea9e5af34c4e9dcbca37384cb302759553c42e000cfc5a4546ca2afdc989ea073d745bfa6d99e7a81bed32cffe2c903ccd49dc60d0032897fe0154346a4a6c01";

#[test]
fun test_siwe_message_reference() {
    let nonce = make32(0x22);
    let msg = siwe::build_message(
        @0x1,                       // registry domain
        ETH_ADDRESS,
        @0x2,                       // session key
        0,                          // generation
        nonce,
        1700000000000,              // expires_at_ms
        1,                          // chain id
        b"2026-06-09T00:00:00.000Z",
    );
    let expected = b"siws-session.demo wants you to sign in with your Ethereum account:\n0x1a642f0E3c3aF545E7AcBD38b07251B3990914F1\n\nAuthorize a Sui session key.\n\nURI: https://siws-session.demo\nVersion: 1\nChain ID: 1\nNonce: 2222222222222222222222222222222222222222222222222222222222222222\nIssued At: 2026-06-09T00:00:00.000Z\nResources:\n- siws-session://sui-registry/0x0000000000000000000000000000000000000000000000000000000000000001\n- siws-session://session-key/0x0000000000000000000000000000000000000000000000000000000000000002\n- siws-session://generation/0\n- siws-session://expires/1700000000000";
    assert!(msg == expected, 0);
}

#[test]
fun test_recover_eth_address() {
    let nonce = make32(0x22);
    let msg = siwe::build_message(
        @0x1, ETH_ADDRESS, @0x2, 0, nonce, 1700000000000, 1,
        b"2026-06-09T00:00:00.000Z",
    );
    // The signature recovers exactly the signer's address (full EIP-191 +
    // ecrecover + keccak address-derivation path).
    let recovered = siwe::recover_eth_address(SIG65, msg);
    assert!(recovered == ETH_ADDRESS, 0);
}

fun make32(v: u8): vector<u8> {
    let mut out = vector::empty<u8>();
    let mut i = 0;
    while (i < 32) { out.push_back(v); i = i + 1; };
    out
}
