/// Golden-fixture tests for the Move wire codec: the hex literals are
/// the committed fixtures from `rust-backend/crates/vault-messages/
/// fixtures/` byte-for-byte. A failure here means the Move codec
/// drifted from the canonical layout — fix the codec (or deliberately
/// re-bless ALL THREE codecs and the fixtures together).
#[test_only]
module vault_v2::wire_tests;

use vault_v2::wire;

fun addr(tag: u8): address {
    let mut bytes = vector[];
    bytes.push_back(0xa0 | tag);
    let mut i = 1;
    while (i < 31) {
        bytes.push_back(0);
        i = i + 1;
    };
    bytes.push_back(tag);
    sui::address::from_bytes(bytes)
}

const DEPOSIT_NOTICE_HEX: vector<u8> =
    x"010100000000000001010000000000000001a200000000000000000000000000000000000000000000000000000000000002a100000000000000000000000000000000000000000000000000000000000001000000000000000700000000000000030000000000000029ad0000000000000000000000000000000000000000000000000000000000000d01000000000000000003782dace9d900010200000198f1991e2b";
const WITHDRAW_REQUEST_HEX: vector<u8> =
    x"010200000000000001010000000000000001a200000000000000000000000000000000000000000000000000000000000002a100000000000000000000000000000000000000000000000000000000000001000000000000000800000000000000030000000000000009ad0000000000000000000000000000000000000000000000000000000000000d010102030405060708090a0b0c0d0e0f1000";
const PAYOUT_RECEIPT_HEX: vector<u8> =
    x"010300000000000001010000000000000001a200000000000000000000000000000000000000000000000000000000000002a1000000000000000000000000000000000000000000000000000000000000010000000000000009000000000000000300000000000000090000000000000000000000003b9aca07";
const STATE_SYNC_HEX: vector<u8> =
    x"010400000000000001010000000000000001a200000000000000000000000000000000000000000000000000000000000002a100000000000000000000000000000000000000000000000000000000000001000000000000000a000000000000000302010000000000000000000000012a05f200000000000000000000000000000004d202000000000000000000000000000000000000000000000001000000000000000000000000000000000ac875621e7a80000004deadbeef00000198f78efd7b";
const DEPOSIT_ACK_HEX: vector<u8> =
    x"010500000000000001010000000000000001a200000000000000000000000000000000000000000000000000000000000002a100000000000000000000000000000000000000000000000000000000000001000000000000000b000000000000002901000000000000000001b69b4ba630f34e";
const WITHDRAW_ACK_HEX: vector<u8> =
    x"010600000000000001010000000000000001a200000000000000000000000000000000000000000000000000000000000002a100000000000000000000000000000000000000000000000000000000000001000000000000000c0000000000000009ad0000000000000000000000000000000000000000000000000000000000000d0000000000000000000000003b9ac9ff";
const CONFIG_SYNC_HEX: vector<u8> =
    x"010700000000000001010000000000000001a200000000000000000000000000000000000000000000000000000000000002a100000000000000000000000000000000000000000000000000000000000001000000000000000d0001ac0000000000000000000000000000000000000000000000000000000000000c01ae0000000000000000000000000000000000000000000000000000000000000e";

#[test]
fun decode_deposit_notice_golden() {
    let (env, inbound) = wire::decode_inbound(DEPOSIT_NOTICE_HEX);
    assert!(wire::msg_type(&env) == wire::deposit_notice_type());
    assert!(wire::src_chain_id(&env) == 0x101);
    assert!(wire::dst_chain_id(&env) == 1);
    assert!(wire::src_app(&env) == addr(2));
    assert!(wire::dst_app(&env) == addr(1));
    assert!(wire::seq(&env) == 7);
    let (spoke_id, deposit_seq, depositor, asset, amount, tranche, ts_ms) =
        wire::as_deposit_notice(&inbound);
    assert!(spoke_id == 3);
    assert!(deposit_seq == 41);
    assert!(depositor == addr(0xd));
    assert!(asset == 1);
    assert!(amount == 250_000_000_000_000_001);
    assert!(tranche == 2);
    assert!(ts_ms == 1_756_400_000_555);
}

#[test]
fun decode_withdraw_request_golden() {
    let (env, inbound) = wire::decode_inbound(WITHDRAW_REQUEST_HEX);
    assert!(wire::seq(&env) == 8);
    let (spoke_id, request_seq, user, tranche, shares, all) =
        wire::as_withdraw_request(&inbound);
    assert!(spoke_id == 3);
    assert!(request_seq == 9);
    assert!(user == addr(0xd));
    assert!(tranche == 1);
    assert!(shares == 0x0102030405060708090a0b0c0d0e0f10);
    assert!(!all);
}

#[test]
fun decode_payout_receipt_golden() {
    let (env, inbound) = wire::decode_inbound(PAYOUT_RECEIPT_HEX);
    assert!(wire::seq(&env) == 9);
    let (spoke_id, request_seq, amount) = wire::as_payout_receipt(&inbound);
    assert!(spoke_id == 3);
    assert!(request_seq == 9);
    assert!(amount == 1_000_000_007);
}

#[test]
fun decode_state_sync_golden() {
    let (env, inbound) = wire::decode_inbound(STATE_SYNC_HEX);
    assert!(wire::seq(&env) == 10);
    let (spoke_id, codes, frees, reserveds, fee_pot, raw, ts_ms) =
        wire::as_state_sync(&inbound);
    assert!(spoke_id == 3);
    assert!(codes == vector[1, 2]);
    assert!(frees == vector[5_000_000_000, 0]);
    assert!(reserveds == vector[1_234, 18_446_744_073_709_551_616]);
    assert!(fee_pot == 777_000_000_000_000_000);
    assert!(raw == x"deadbeef");
    assert!(ts_ms == 1_756_500_000_123);
}

#[test]
fun encode_acks_and_config_golden() {
    let ack = wire::encode_deposit_ack(
        0x101, 1, addr(2), addr(1), 11, 41, true, 123_456_789_012_345_678,
    );
    assert!(ack == DEPOSIT_ACK_HEX);

    let wack = wire::encode_withdraw_ack(0x101, 1, addr(2), addr(1), 12, 9, addr(0xd), 999_999_999);
    assert!(wack == WITHDRAW_ACK_HEX);

    let cs = wire::encode_config_sync(
        0x101, 1, addr(2), addr(1), 13, false, true, addr(0xc), 1, addr(0xe),
    );
    assert!(cs == CONFIG_SYNC_HEX);
}

#[test]
fun test_encoders_round_trip() {
    let bytes = wire::encode_deposit_notice_for_testing(
        0x101, 1, addr(2), addr(1), 7, 3, 41, addr(0xd), 1, 250_000_000_000_000_001, 2,
        1_756_400_000_555,
    );
    assert!(bytes == DEPOSIT_NOTICE_HEX);
    let bytes = wire::encode_withdraw_request_for_testing(
        0x101, 1, addr(2), addr(1), 8, 3, 9, addr(0xd), 1, 0x0102030405060708090a0b0c0d0e0f10,
        false,
    );
    assert!(bytes == WITHDRAW_REQUEST_HEX);
    let bytes =
        wire::encode_payout_receipt_for_testing(0x101, 1, addr(2), addr(1), 9, 3, 9, 1_000_000_007);
    assert!(bytes == PAYOUT_RECEIPT_HEX);
    let bytes = wire::encode_state_sync_for_testing(
        0x101, 1, addr(2), addr(1), 10, 3,
        vector[1, 2],
        vector[5_000_000_000, 0],
        vector[1_234, 18_446_744_073_709_551_616],
        777_000_000_000_000_000,
        x"deadbeef",
        1_756_500_000_123,
    );
    assert!(bytes == STATE_SYNC_HEX);
}

#[test]
#[expected_failure(abort_code = 144, location = vault_v2::wire)]
fun decode_rejects_bad_version() {
    let mut bytes = DEPOSIT_NOTICE_HEX;
    *&mut bytes[0] = 9;
    let (_, _) = wire::decode_inbound(bytes);
}

#[test]
#[expected_failure(abort_code = 144, location = vault_v2::wire)]
fun decode_rejects_hub_to_spoke_type() {
    let (_, _) = wire::decode_inbound(DEPOSIT_ACK_HEX);
}

#[test]
#[expected_failure(abort_code = 144, location = vault_v2::wire)]
fun decode_rejects_trailing_bytes() {
    let mut bytes = DEPOSIT_NOTICE_HEX;
    bytes.push_back(0);
    let (_, _) = wire::decode_inbound(bytes);
}

#[test]
#[expected_failure(abort_code = 144, location = vault_v2::wire)]
fun decode_rejects_truncation() {
    let mut bytes = DEPOSIT_NOTICE_HEX;
    bytes.pop_back();
    let (_, _) = wire::decode_inbound(bytes);
}

#[test]
#[expected_failure(abort_code = 144, location = vault_v2::wire)]
fun decode_rejects_bad_bool() {
    let mut bytes = WITHDRAW_REQUEST_HEX;
    let last = bytes.length() - 1;
    *&mut bytes[last] = 2;
    let (_, _) = wire::decode_inbound(bytes);
}
