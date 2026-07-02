/// Layer 2 transfer payload — the bytes a Locker carries in
/// `CrossChainMessage.payload` (bridge-spec.md §3.2/§3.3). Byte-identical to the
/// Rust `bridge_types::transfer` and Solidity `TransferPayload` codecs.
///
/// Fixed big-endian packed layout (72 bytes):
///   asset_id   bytes32           32
///   amount     u64  big-endian    8   (wire amount, fixed WIRE_DECIMALS)
///   recipient  bytes32           32
///
/// `amount` is a wire amount in a shared decimal precision; the Locker scales
/// to/from local decimals (NTT trimmed-amount). This codec just carries the u64.
module locker::transfer_payload;

const BYTES32_LEN: u64 = 32;
const ENCODED_LEN: u64 = 72;

/// Shared wire precision for cross-chain amounts.
const WIRE_DECIMALS: u8 = 8;

const EBadBytes32Length: u64 = 1;
const EBadPayloadLength: u64 = 2;

public struct TransferPayload has copy, drop, store {
    asset_id: vector<u8>,
    amount: u64,
    recipient: vector<u8>,
}

public fun wire_decimals(): u8 { WIRE_DECIMALS }

public fun new(asset_id: vector<u8>, amount: u64, recipient: vector<u8>): TransferPayload {
    assert!(asset_id.length() == BYTES32_LEN, EBadBytes32Length);
    assert!(recipient.length() == BYTES32_LEN, EBadBytes32Length);
    TransferPayload { asset_id, amount, recipient }
}

public fun asset_id(p: &TransferPayload): vector<u8> { p.asset_id }
public fun amount(p: &TransferPayload): u64 { p.amount }
public fun recipient(p: &TransferPayload): vector<u8> { p.recipient }

public fun encode(p: &TransferPayload): vector<u8> {
    let mut out = p.asset_id;
    append_u64_be(&mut out, p.amount);
    out.append(p.recipient);
    out
}

public fun decode(bytes: vector<u8>): TransferPayload {
    assert!(bytes.length() == ENCODED_LEN, EBadPayloadLength);
    let asset_id = slice(&bytes, 0, BYTES32_LEN);
    let amount = read_u64_be(&bytes, BYTES32_LEN);
    let recipient = slice(&bytes, 40, BYTES32_LEN);
    TransferPayload { asset_id, amount, recipient }
}

fun append_u64_be(out: &mut vector<u8>, v: u64) {
    let mut i = 8u8;
    while (i > 0) {
        i = i - 1;
        out.push_back(((v >> (8 * i)) & 0xff) as u8);
    };
}

fun read_u64_be(b: &vector<u8>, off: u64): u64 {
    let mut v = 0u64;
    let mut i = 0;
    while (i < 8) {
        v = (v << 8) | (*b.borrow(off + i) as u64);
        i = i + 1;
    };
    v
}

fun slice(b: &vector<u8>, off: u64, len: u64): vector<u8> {
    let mut out = vector<u8>[];
    let mut i = 0;
    while (i < len) {
        out.push_back(*b.borrow(off + i));
        i = i + 1;
    };
    out
}

#[test_only]
fun filled(byte: u8, n: u64): vector<u8> {
    let mut v = vector<u8>[];
    let mut i = 0;
    while (i < n) { v.push_back(byte); i = i + 1; };
    v
}

#[test]
fun known_encoding_vector() {
    let p = new(filled(0x11, 32), 123_456_789, filled(0x22, 32));
    let expected =
        x"111111111111111111111111111111111111111111111111111111111111111100000000075bcd152222222222222222222222222222222222222222222222222222222222222222";
    assert!(encode(&p) == expected, 0);
    assert!(encode(&p).length() == ENCODED_LEN, 1);
}

#[test]
fun round_trips() {
    let p = new(filled(0xab, 32), 18446744073709551615, filled(0xcd, 32));
    let d = decode(encode(&p));
    assert!(d.asset_id == p.asset_id, 0);
    assert!(d.amount == p.amount, 1);
    assert!(d.recipient == p.recipient, 2);
}

#[test]
#[expected_failure(abort_code = EBadPayloadLength)]
fun rejects_bad_length() {
    decode(filled(0, 71));
}
