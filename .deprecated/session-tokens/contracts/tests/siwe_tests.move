#[test_only]
module siws_session::siwe_tests;

use siws_session::siwe;

// Reference vector generated with a real secp256k1 key (priv = 0x01 * 32) over
// the EIP-191 personal_sign of the message below. Mirrored byte-for-byte in
// `frontend/siws-session-sdk/src/siwe.test.ts`. Regenerate with `frontend/siws-session-sdk/gen-siwe.mjs`.

const ETH_ADDRESS: vector<u8> = x"1a642f0e3c3af545e7acbd38b07251b3990914f1";
const SIG65: vector<u8> = x"d6ccd719f17ee783d234ce54502d4ce1de7c30ce71f9f824238b85dd47ab2d541f13be8689030032d7ac8ff193a6726d5965f4c1767824357fa5985f81bfb51901";
// Real secp256k1 signature (same priv = 0x01 * 32) over the withdraw message.
const WITHDRAW_SIG65: vector<u8> = x"1e5414afa71e6ee4550ccddf7f7277fa1843f267fa842f20a3cab8487f6b10a14b102c3391eef116e2bd72f84f6ec1adb5e6e5e6af79d36b892a3d0d94dee4fa01";

fun limit_types(): vector<vector<u8>> {
    vector[
        b"0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI",
        b"0x00000000000000000000000000000000000000000000000000000000000000aa::tusdc::TUSDC",
    ]
}

fun reference_message(): vector<u8> {
    siwe::build_message(
        @0x1,                       // registry domain
        ETH_ADDRESS,
        @0x2,                       // session key
        0,                          // generation
        make32(0x22),               // nonce
        1700000000000,              // expires_at_ms
        1,                          // chain id
        b"2026-06-09T00:00:00.000Z",
        &limit_types(),
        &vector[200, 1000000],
        &vector[500, 5000000],
    )
}

#[test]
fun test_siwe_message_reference() {
    let expected = b"siws-session.demo wants you to sign in with your Ethereum account:\n0x1a642f0E3c3aF545E7AcBD38b07251B3990914F1\n\nAuthorize a Sui session key.\n\nURI: https://siws-session.demo\nVersion: 1\nChain ID: 1\nNonce: 2222222222222222222222222222222222222222222222222222222222222222\nIssued At: 2026-06-09T00:00:00.000Z\nResources:\n- siws-session://sui-registry/0x0000000000000000000000000000000000000000000000000000000000000001\n- siws-session://session-key/0x0000000000000000000000000000000000000000000000000000000000000002\n- siws-session://generation/0\n- siws-session://expires/1700000000000\n- siws-session://limits/0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI=200/500,0x00000000000000000000000000000000000000000000000000000000000000aa::tusdc::TUSDC=1000000/5000000";
    assert!(reference_message() == expected, 0);
}

#[test]
fun test_recover_eth_address() {
    // The signature recovers exactly the signer's address (full EIP-191 +
    // ecrecover + keccak address-derivation path).
    let recovered = siwe::recover_eth_address(SIG65, reference_message());
    assert!(recovered == ETH_ADDRESS, 0);
}

fun reference_withdraw_message(): vector<u8> {
    siwe::build_withdraw_message(
        @0x1,                       // registry domain
        ETH_ADDRESS,
        @0x3,                       // options custody account id
        b"0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI",
        250000,                     // amount
        @0x4,                       // recipient
        make32(0x22),               // nonce
        1700000000000,              // expires_at_ms
        1,                          // chain id
        b"2026-06-09T00:00:00.000Z",
    )
}

#[test]
fun test_siwe_withdraw_message_reference() {
    let expected = b"siws-session.demo wants you to sign in with your Ethereum account:\n0x1a642f0E3c3aF545E7AcBD38b07251B3990914F1\n\nAuthorize a Sui withdrawal.\n\nURI: https://siws-session.demo\nVersion: 1\nChain ID: 1\nNonce: 2222222222222222222222222222222222222222222222222222222222222222\nIssued At: 2026-06-09T00:00:00.000Z\nResources:\n- siws-session://sui-registry/0x0000000000000000000000000000000000000000000000000000000000000001\n- siws-session://account/0x0000000000000000000000000000000000000000000000000000000000000003\n- siws-session://coin-type/0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI\n- siws-session://amount/250000\n- siws-session://recipient/0x0000000000000000000000000000000000000000000000000000000000000004\n- siws-session://expires/1700000000000";
    assert!(reference_withdraw_message() == expected, 0);
}

#[test]
fun test_recover_eth_address_withdraw() {
    // The real signature over the withdraw message recovers the signer's
    // address — exercises `verify_withdraw_auth_eth`'s full crypto path.
    let recovered = siwe::recover_eth_address(WITHDRAW_SIG65, reference_withdraw_message());
    assert!(recovered == ETH_ADDRESS, 0);
}

fun make32(v: u8): vector<u8> {
    let mut out = vector::empty<u8>();
    let mut i = 0;
    while (i < 32) { out.push_back(v); i = i + 1; };
    out
}
