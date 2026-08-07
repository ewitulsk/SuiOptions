#[test_only]
module exchange::order_tests;

use std::string;
use exchange::order;

const MAKER: address = @0xA1;

fun sample(): order::Order {
    order::new_for_testing(
        string::utf8(b"0x2::sui::SUI"),
        string::utf8(b"0xaa::usdc::USDC"),
        100,
        200,
        10,
        MAKER,
        @0x71.to_id(),
        @0x0,
        @0x0,
        1000,
        1,
    )
}

#[test]
fun bcs_roundtrip() {
    let ord = sample();
    let bytes = order::to_bytes(&ord);
    let back = order::from_bytes(bytes);
    assert!(order::to_bytes(&back) == bytes, 0);
    assert!(back.maker() == MAKER, 1);
    assert!(back.maker_amount() == 100, 2);
    assert!(back.taker_amount() == 200, 3);
    assert!(back.salt() == 1, 4);
}

#[test, expected_failure(abort_code = order::ETrailingBytes)]
fun trailing_bytes_rejected() {
    let mut bytes = order::to_bytes(&sample());
    bytes.push_back(0);
    order::from_bytes(bytes);
}

#[test, expected_failure(abort_code = order::EUnsupportedScheme)]
fun unknown_scheme_rejected() {
    let d = x"0000000000000000000000000000000000000000000000000000000000000000";
    let mut sig = vector[0x03u8]; // zkLogin & friends: rejected
    let mut i = 0u64;
    while (i < 64) {
        sig.push_back(0);
        i = i + 1;
    };
    order::verify_signature(&d, &sig, &vector[]);
}

#[test, expected_failure(abort_code = order::EBadSignatureLength)]
fun short_signature_rejected() {
    let d = x"0000000000000000000000000000000000000000000000000000000000000000";
    order::verify_signature(&d, &vector[0x00u8], &vector[]);
}

#[test, expected_failure(abort_code = order::ENotLowS)]
fun high_s_rejected() {
    // s half (bytes 32..64) all-0xff is > n/2.
    let d = x"0000000000000000000000000000000000000000000000000000000000000000";
    let mut sig = vector[0x01u8];
    let mut i = 0u64;
    while (i < 32) {
        sig.push_back(0x01);
        i = i + 1;
    };
    while (i < 64) {
        sig.push_back(0xff);
        i = i + 1;
    };
    let mut pk = vector[0x02u8];
    i = 0;
    while (i < 32) {
        pk.push_back(0x01);
        i = i + 1;
    };
    order::verify_signature(&d, &sig, &pk);
}

#[test]
fun canonical_type_is_prefixed_long_form() {
    let s = order::canonical_type<sui::sui::SUI>();
    assert!(
        s == string::utf8(
            b"0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI",
        ),
        0,
    );
}
