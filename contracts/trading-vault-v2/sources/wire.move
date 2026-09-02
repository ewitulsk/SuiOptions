/// Move side of the canonical multichain wire format. The layout's
/// source of truth is `rust-backend/crates/vault-messages` (see its
/// module docs); the golden fixtures under that crate's `fixtures/` are
/// mirrored byte-for-byte in `tests/wire_tests.move`. All integers are
/// fixed-width big-endian; addresses are 32 bytes.
///
/// The hub DECODES spoke→hub types (1–4) and ENCODES hub→spoke types
/// (5–7); the reverse directions exist only in the EVM and Rust codecs.
module vault_v2::wire;

use vault_v2::errors;

const WIRE_VERSION: u8 = 1;
const ENVELOPE_LEN: u64 = 90;

const MSG_DEPOSIT_NOTICE: u8 = 1;
const MSG_WITHDRAW_REQUEST: u8 = 2;
const MSG_PAYOUT_RECEIPT: u8 = 3;
const MSG_STATE_SYNC: u8 = 4;
const MSG_DEPOSIT_ACK: u8 = 5;
const MSG_WITHDRAW_ACK: u8 = 6;
const MSG_CONFIG_SYNC: u8 = 7;

const MAX_INTEGRATION_RAW_LEN: u64 = 4096;
const MAX_STATE_SYNC_ASSETS: u64 = 16;

public struct Envelope has copy, drop {
    msg_type: u8,
    src_chain_id: u64,
    dst_chain_id: u64,
    src_app: address,
    dst_app: address,
    seq: u64,
}

/// Decoded spoke→hub payload. One enum rather than per-type structs so
/// `multichain` can dispatch on it after the shared envelope checks.
public enum Inbound has copy, drop {
    DepositNotice {
        spoke_id: u64,
        deposit_seq: u64,
        depositor: address,
        asset: u8,
        amount: u128,
        tranche: u8,
        ts_ms: u64,
    },
    WithdrawRequest {
        spoke_id: u64,
        request_seq: u64,
        user: address,
        tranche: u8,
        shares: u128,
        all: bool,
    },
    PayoutReceipt { spoke_id: u64, request_seq: u64, amount: u128 },
    StateSync {
        spoke_id: u64,
        asset_codes: vector<u8>,
        frees: vector<u128>,
        reserveds: vector<u128>,
        fee_pot_balance: u128,
        integration_raw: vector<u8>,
        ts_ms: u64,
    },
}

// ─────────────────────────────── decode ───────────────────────────────

public struct Reader has copy, drop { buf: vector<u8>, pos: u64 }

fun rd_u8(r: &mut Reader): u8 {
    assert!(r.pos < r.buf.length(), errors::wire_malformed());
    let b = r.buf[r.pos];
    r.pos = r.pos + 1;
    b
}

fun rd_bool(r: &mut Reader): bool {
    let b = rd_u8(r);
    assert!(b <= 1, errors::wire_malformed());
    b == 1
}

fun rd_uint(r: &mut Reader, bytes: u64): u256 {
    assert!(r.pos + bytes <= r.buf.length(), errors::wire_malformed());
    let mut v: u256 = 0;
    let mut i = 0;
    while (i < bytes) {
        v = (v << 8) | (r.buf[r.pos + i] as u256);
        i = i + 1;
    };
    r.pos = r.pos + bytes;
    v
}

fun rd_u16(r: &mut Reader): u16 { (rd_uint(r, 2) as u16) }

fun rd_u64(r: &mut Reader): u64 { (rd_uint(r, 8) as u64) }

fun rd_u128(r: &mut Reader): u128 { (rd_uint(r, 16) as u128) }

fun rd_address(r: &mut Reader): address {
    assert!(r.pos + 32 <= r.buf.length(), errors::wire_malformed());
    let mut bytes = vector[];
    let mut i = 0;
    while (i < 32) {
        bytes.push_back(r.buf[r.pos + i]);
        i = i + 1;
    };
    r.pos = r.pos + 32;
    sui::address::from_bytes(bytes)
}

/// Decode a full spoke→hub message; aborts `wire_malformed` on any
/// deviation from the canonical layout (including trailing bytes).
public fun decode_inbound(bytes: vector<u8>): (Envelope, Inbound) {
    assert!(bytes.length() >= ENVELOPE_LEN, errors::wire_malformed());
    let mut r = Reader { buf: bytes, pos: 0 };
    assert!(rd_u8(&mut r) == WIRE_VERSION, errors::wire_malformed());
    let msg_type = rd_u8(&mut r);
    let envelope = Envelope {
        msg_type,
        src_chain_id: rd_u64(&mut r),
        dst_chain_id: rd_u64(&mut r),
        src_app: rd_address(&mut r),
        dst_app: rd_address(&mut r),
        seq: rd_u64(&mut r),
    };
    let inbound = if (msg_type == MSG_DEPOSIT_NOTICE) {
        Inbound::DepositNotice {
            spoke_id: rd_u64(&mut r),
            deposit_seq: rd_u64(&mut r),
            depositor: rd_address(&mut r),
            asset: rd_u8(&mut r),
            amount: rd_u128(&mut r),
            tranche: rd_u8(&mut r),
            ts_ms: rd_u64(&mut r),
        }
    } else if (msg_type == MSG_WITHDRAW_REQUEST) {
        Inbound::WithdrawRequest {
            spoke_id: rd_u64(&mut r),
            request_seq: rd_u64(&mut r),
            user: rd_address(&mut r),
            tranche: rd_u8(&mut r),
            shares: rd_u128(&mut r),
            all: rd_bool(&mut r),
        }
    } else if (msg_type == MSG_PAYOUT_RECEIPT) {
        Inbound::PayoutReceipt {
            spoke_id: rd_u64(&mut r),
            request_seq: rd_u64(&mut r),
            amount: rd_u128(&mut r),
        }
    } else if (msg_type == MSG_STATE_SYNC) {
        let spoke_id = rd_u64(&mut r);
        let n = rd_u8(&mut r) as u64;
        assert!(n <= MAX_STATE_SYNC_ASSETS, errors::wire_malformed());
        let mut asset_codes = vector[];
        let mut frees = vector[];
        let mut reserveds = vector[];
        let mut i = 0;
        while (i < n) {
            asset_codes.push_back(rd_u8(&mut r));
            frees.push_back(rd_u128(&mut r));
            reserveds.push_back(rd_u128(&mut r));
            i = i + 1;
        };
        let fee_pot_balance = rd_u128(&mut r);
        let raw_len = rd_u16(&mut r) as u64;
        assert!(raw_len <= MAX_INTEGRATION_RAW_LEN, errors::wire_malformed());
        assert!(r.pos + raw_len <= r.buf.length(), errors::wire_malformed());
        let mut integration_raw = vector[];
        let mut j = 0;
        while (j < raw_len) {
            integration_raw.push_back(r.buf[r.pos + j]);
            j = j + 1;
        };
        r.pos = r.pos + raw_len;
        Inbound::StateSync {
            spoke_id,
            asset_codes,
            frees,
            reserveds,
            fee_pot_balance,
            integration_raw,
            ts_ms: rd_u64(&mut r),
        }
    } else {
        abort errors::wire_malformed()
    };
    assert!(r.pos == r.buf.length(), errors::wire_malformed());
    (envelope, inbound)
}

// ─────────────────────────────── encode ───────────────────────────────

fun wr_uint(out: &mut vector<u8>, v: u256, bytes: u64) {
    let mut i = bytes;
    while (i > 0) {
        i = i - 1;
        out.push_back((((v >> ((i * 8) as u8)) & 0xff) as u8));
    };
}

fun wr_envelope(
    out: &mut vector<u8>,
    msg_type: u8,
    src_chain_id: u64,
    dst_chain_id: u64,
    src_app: address,
    dst_app: address,
    seq: u64,
) {
    out.push_back(WIRE_VERSION);
    out.push_back(msg_type);
    wr_uint(out, src_chain_id as u256, 8);
    wr_uint(out, dst_chain_id as u256, 8);
    out.append(src_app.to_bytes());
    out.append(dst_app.to_bytes());
    wr_uint(out, seq as u256, 8);
}

public fun encode_deposit_ack(
    src_chain_id: u64,
    dst_chain_id: u64,
    src_app: address,
    dst_app: address,
    seq: u64,
    deposit_seq: u64,
    accepted: bool,
    shares: u128,
): vector<u8> {
    let mut out = vector[];
    wr_envelope(&mut out, MSG_DEPOSIT_ACK, src_chain_id, dst_chain_id, src_app, dst_app, seq);
    wr_uint(&mut out, deposit_seq as u256, 8);
    out.push_back(if (accepted) { 1 } else { 0 });
    wr_uint(&mut out, shares as u256, 16);
    out
}

public fun encode_withdraw_ack(
    src_chain_id: u64,
    dst_chain_id: u64,
    src_app: address,
    dst_app: address,
    seq: u64,
    request_seq: u64,
    user: address,
    pay_amount: u128,
): vector<u8> {
    let mut out = vector[];
    wr_envelope(&mut out, MSG_WITHDRAW_ACK, src_chain_id, dst_chain_id, src_app, dst_app, seq);
    wr_uint(&mut out, request_seq as u256, 8);
    out.append(user.to_bytes());
    wr_uint(&mut out, pay_amount as u256, 16);
    out
}

public fun encode_config_sync(
    src_chain_id: u64,
    dst_chain_id: u64,
    src_app: address,
    dst_app: address,
    seq: u64,
    paused: bool,
    risk_off: bool,
    curator: address,
    endpoint_code: u8,
    integrations_root: address,
): vector<u8> {
    let mut out = vector[];
    wr_envelope(&mut out, MSG_CONFIG_SYNC, src_chain_id, dst_chain_id, src_app, dst_app, seq);
    out.push_back(if (paused) { 1 } else { 0 });
    out.push_back(if (risk_off) { 1 } else { 0 });
    out.append(curator.to_bytes());
    out.push_back(endpoint_code);
    out.append(integrations_root.to_bytes());
    out
}

// ───────────────────── test-only inbound encoders ─────────────────────
// Production spoke→hub encoding lives in the EVM codec; these exist so
// Move tests can craft canonical messages against live object ids.

#[test_only]
public fun encode_deposit_notice_for_testing(
    src_chain_id: u64,
    dst_chain_id: u64,
    src_app: address,
    dst_app: address,
    seq: u64,
    spoke_id: u64,
    deposit_seq: u64,
    depositor: address,
    asset: u8,
    amount: u128,
    tranche: u8,
    ts_ms: u64,
): vector<u8> {
    let mut out = vector[];
    wr_envelope(&mut out, MSG_DEPOSIT_NOTICE, src_chain_id, dst_chain_id, src_app, dst_app, seq);
    wr_uint(&mut out, spoke_id as u256, 8);
    wr_uint(&mut out, deposit_seq as u256, 8);
    out.append(depositor.to_bytes());
    out.push_back(asset);
    wr_uint(&mut out, amount as u256, 16);
    out.push_back(tranche);
    wr_uint(&mut out, ts_ms as u256, 8);
    out
}

#[test_only]
public fun encode_withdraw_request_for_testing(
    src_chain_id: u64,
    dst_chain_id: u64,
    src_app: address,
    dst_app: address,
    seq: u64,
    spoke_id: u64,
    request_seq: u64,
    user: address,
    tranche: u8,
    shares: u128,
    all: bool,
): vector<u8> {
    let mut out = vector[];
    wr_envelope(&mut out, MSG_WITHDRAW_REQUEST, src_chain_id, dst_chain_id, src_app, dst_app, seq);
    wr_uint(&mut out, spoke_id as u256, 8);
    wr_uint(&mut out, request_seq as u256, 8);
    out.append(user.to_bytes());
    out.push_back(tranche);
    wr_uint(&mut out, shares as u256, 16);
    out.push_back(if (all) { 1 } else { 0 });
    out
}

#[test_only]
public fun encode_payout_receipt_for_testing(
    src_chain_id: u64,
    dst_chain_id: u64,
    src_app: address,
    dst_app: address,
    seq: u64,
    spoke_id: u64,
    request_seq: u64,
    amount: u128,
): vector<u8> {
    let mut out = vector[];
    wr_envelope(&mut out, MSG_PAYOUT_RECEIPT, src_chain_id, dst_chain_id, src_app, dst_app, seq);
    wr_uint(&mut out, spoke_id as u256, 8);
    wr_uint(&mut out, request_seq as u256, 8);
    wr_uint(&mut out, amount as u256, 16);
    out
}

#[test_only]
public fun encode_state_sync_for_testing(
    src_chain_id: u64,
    dst_chain_id: u64,
    src_app: address,
    dst_app: address,
    seq: u64,
    spoke_id: u64,
    asset_codes: vector<u8>,
    frees: vector<u128>,
    reserveds: vector<u128>,
    fee_pot_balance: u128,
    integration_raw: vector<u8>,
    ts_ms: u64,
): vector<u8> {
    let mut out = vector[];
    wr_envelope(&mut out, MSG_STATE_SYNC, src_chain_id, dst_chain_id, src_app, dst_app, seq);
    wr_uint(&mut out, spoke_id as u256, 8);
    out.push_back(asset_codes.length() as u8);
    let mut i = 0;
    while (i < asset_codes.length()) {
        out.push_back(asset_codes[i]);
        wr_uint(&mut out, frees[i] as u256, 16);
        wr_uint(&mut out, reserveds[i] as u256, 16);
        i = i + 1;
    };
    wr_uint(&mut out, fee_pot_balance as u256, 16);
    wr_uint(&mut out, integration_raw.length() as u256, 2);
    out.append(integration_raw);
    wr_uint(&mut out, ts_ms as u256, 8);
    out
}

// ─────────────────────────── inbound accessors ───────────────────────────
// Enum variants are only matchable here, so `multichain` dispatches on
// the envelope's msg_type and extracts with these (abort on mismatch).

public fun inbound_spoke_id(i: &Inbound): u64 {
    match (i) {
        Inbound::DepositNotice { spoke_id, .. } => *spoke_id,
        Inbound::WithdrawRequest { spoke_id, .. } => *spoke_id,
        Inbound::PayoutReceipt { spoke_id, .. } => *spoke_id,
        Inbound::StateSync { spoke_id, .. } => *spoke_id,
    }
}

/// (spoke_id, deposit_seq, depositor, asset, amount, tranche, ts_ms)
public fun as_deposit_notice(i: &Inbound): (u64, u64, address, u8, u128, u8, u64) {
    match (i) {
        Inbound::DepositNotice { spoke_id, deposit_seq, depositor, asset, amount, tranche, ts_ms } =>
            (*spoke_id, *deposit_seq, *depositor, *asset, *amount, *tranche, *ts_ms),
        _ => abort errors::wire_malformed(),
    }
}

/// (spoke_id, request_seq, user, tranche, shares, all)
public fun as_withdraw_request(i: &Inbound): (u64, u64, address, u8, u128, bool) {
    match (i) {
        Inbound::WithdrawRequest { spoke_id, request_seq, user, tranche, shares, all } =>
            (*spoke_id, *request_seq, *user, *tranche, *shares, *all),
        _ => abort errors::wire_malformed(),
    }
}

/// (spoke_id, request_seq, amount)
public fun as_payout_receipt(i: &Inbound): (u64, u64, u128) {
    match (i) {
        Inbound::PayoutReceipt { spoke_id, request_seq, amount } =>
            (*spoke_id, *request_seq, *amount),
        _ => abort errors::wire_malformed(),
    }
}

/// (spoke_id, asset_codes, frees, reserveds, fee_pot_balance,
/// integration_raw, ts_ms)
public fun as_state_sync(
    i: &Inbound,
): (u64, vector<u8>, vector<u128>, vector<u128>, u128, vector<u8>, u64) {
    match (i) {
        Inbound::StateSync {
            spoke_id,
            asset_codes,
            frees,
            reserveds,
            fee_pot_balance,
            integration_raw,
            ts_ms,
        } => (
            *spoke_id,
            *asset_codes,
            *frees,
            *reserveds,
            *fee_pot_balance,
            *integration_raw,
            *ts_ms,
        ),
        _ => abort errors::wire_malformed(),
    }
}

// ─────────────────────────── envelope getters ───────────────────────────

public fun msg_type(e: &Envelope): u8 { e.msg_type }

public fun src_chain_id(e: &Envelope): u64 { e.src_chain_id }

public fun dst_chain_id(e: &Envelope): u64 { e.dst_chain_id }

public fun src_app(e: &Envelope): address { e.src_app }

public fun dst_app(e: &Envelope): address { e.dst_app }

public fun seq(e: &Envelope): u64 { e.seq }

public fun deposit_notice_type(): u8 { MSG_DEPOSIT_NOTICE }

public fun withdraw_request_type(): u8 { MSG_WITHDRAW_REQUEST }

public fun payout_receipt_type(): u8 { MSG_PAYOUT_RECEIPT }

public fun state_sync_type(): u8 { MSG_STATE_SYNC }

public fun deposit_ack_type(): u8 { MSG_DEPOSIT_ACK }

public fun withdraw_ack_type(): u8 { MSG_WITHDRAW_ACK }

public fun config_sync_type(): u8 { MSG_CONFIG_SYNC }
