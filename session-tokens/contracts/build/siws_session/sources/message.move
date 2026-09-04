/// Canonical message serializer (spec §1.4). The contract rebuilds the exact
/// bytes the user signed from on-chain/checked values — it NEVER trusts a
/// caller-supplied blob. The SDK reimplements `build_session_message` /
/// `build_revoke_message` byte-for-byte; the `tests/` module pins the output
/// against a fixed reference vector that the SDK test also asserts.
///
/// Format (ASCII, single `\n` separators, no trailing newline):
///
///   siws-session-v2
///   domain: 0x<32-byte registry addr, lowercase hex>
///   chain: sui:<network>
///   account: 0x<32-byte solana ed25519 pubkey, lowercase hex>
///   session_key: 0x<32-byte temp sui addr, lowercase hex>
///   generation: <decimal u64>
///   nonce: 0x<32-byte nonce, lowercase hex>
///   expires_at_ms: <decimal u64>
///   limits: <encoded per-type spend limits — see `encode_limits`>
///
/// v2 adds the `limits:` line so the per-coin-type spend limits the cap will
/// carry are covered by the root signature (in v1 the caps were unsigned
/// args).
///
/// NOTE: we use lowercase hex (not base58) for the Solana pubkey so both sides
/// stay trivially byte-exact; the wallet renders the raw message text.
module siws_session::message;

use sui::address;
use sui::hex;

use siws_session::errors;

const SESSION_PREFIX: vector<u8> = b"siws-session-v2";
const REVOKE_PREFIX: vector<u8> = b"siws-session-revoke-v1";
const WITHDRAW_PREFIX: vector<u8> = b"siws-session-withdraw-v1";

public fun build_session_message(
    domain: address,
    network: vector<u8>,
    solana_pk: vector<u8>,
    session_key: address,
    generation: u64,
    nonce: vector<u8>,
    expires_at_ms: u64,
    limit_types: &vector<vector<u8>>,
    limit_per_tx: &vector<u64>,
    limit_total: &vector<u64>,
): vector<u8> {
    let mut m = SESSION_PREFIX;
    push_field(&mut m, b"domain: ", hex0x(address::to_bytes(domain)));
    push_field(&mut m, b"chain: ", chain_value(network));
    push_field(&mut m, b"account: ", hex0x(solana_pk));
    push_field(&mut m, b"session_key: ", hex0x(address::to_bytes(session_key)));
    push_field(&mut m, b"generation: ", u64_to_ascii(generation));
    push_field(&mut m, b"nonce: ", hex0x(nonce));
    push_field(&mut m, b"expires_at_ms: ", u64_to_ascii(expires_at_ms));
    push_field(&mut m, b"limits: ", encode_limits(limit_types, limit_per_tx, limit_total));
    m
}

/// `<type>=<per_tx>/<total>` entries joined by `,`; `none` when empty.
/// `type` is the canonical `0x…::module::Name` form
/// (`account::canonical_type_bytes`). Shared with the SIWE builder so both
/// roots sign identical limit encodings.
public(package) fun encode_limits(
    limit_types: &vector<vector<u8>>,
    limit_per_tx: &vector<u64>,
    limit_total: &vector<u64>,
): vector<u8> {
    assert!(
        limit_types.length() == limit_per_tx.length()
            && limit_types.length() == limit_total.length(),
        errors::limits_arity_mismatch(),
    );
    if (limit_types.is_empty()) { return b"none" };
    let mut out = vector::empty<u8>();
    let mut i = 0;
    while (i < limit_types.length()) {
        if (i > 0) { out.push_back(44) }; // ','
        out.append(limit_types[i]);
        out.push_back(61); // '='
        out.append(u64_to_ascii(limit_per_tx[i]));
        out.push_back(47); // '/'
        out.append(u64_to_ascii(limit_total[i]));
        i = i + 1;
    };
    out
}

public fun build_revoke_message(
    domain: address,
    network: vector<u8>,
    solana_pk: vector<u8>,
    account_id: ID,
    nonce: vector<u8>,
    expires_at_ms: u64,
): vector<u8> {
    let mut m = REVOKE_PREFIX;
    push_field(&mut m, b"domain: ", hex0x(address::to_bytes(domain)));
    push_field(&mut m, b"chain: ", chain_value(network));
    push_field(&mut m, b"account: ", hex0x(solana_pk));
    push_field(&mut m, b"account_id: ", hex0x(account_id.to_bytes()));
    push_field(&mut m, b"nonce: ", hex0x(nonce));
    push_field(&mut m, b"expires_at_ms: ", u64_to_ascii(expires_at_ms));
    m
}

/// Withdrawal-authorization message (host wallet / Solana root). Domain-
/// separated from the session/revoke prefixes so a signature for one can never
/// be replayed as another. Binds the exact `account_id` (the options custody
/// account), `coin_type`, `amount`, and `recipient` so a sponsor/session key
/// cannot redirect or resize the withdrawal. `coin_type` is the canonical
/// `0x…::module::Name` form (`account::canonical_type_bytes`), appended raw.
public fun build_withdraw_message(
    domain: address,
    network: vector<u8>,
    owner_pk: vector<u8>,
    account_id: ID,
    coin_type: vector<u8>,
    amount: u64,
    recipient: address,
    nonce: vector<u8>,
    expires_at_ms: u64,
): vector<u8> {
    let mut m = WITHDRAW_PREFIX;
    push_field(&mut m, b"domain: ", hex0x(address::to_bytes(domain)));
    push_field(&mut m, b"chain: ", chain_value(network));
    push_field(&mut m, b"account: ", hex0x(owner_pk));
    push_field(&mut m, b"account_id: ", hex0x(account_id.to_bytes()));
    push_field(&mut m, b"coin_type: ", coin_type);
    push_field(&mut m, b"amount: ", u64_to_ascii(amount));
    push_field(&mut m, b"recipient: ", hex0x(address::to_bytes(recipient)));
    push_field(&mut m, b"nonce: ", hex0x(nonce));
    push_field(&mut m, b"expires_at_ms: ", u64_to_ascii(expires_at_ms));
    m
}

/// Append `\n` + label + value to the running message.
fun push_field(m: &mut vector<u8>, label: vector<u8>, value: vector<u8>) {
    m.push_back(10); // '\n'
    m.append(label);
    m.append(value);
}

fun chain_value(network: vector<u8>): vector<u8> {
    let mut v = b"sui:";
    v.append(network);
    v
}

/// `0x` + lowercase hex of `bytes`.
fun hex0x(bytes: vector<u8>): vector<u8> {
    let mut out = b"0x";
    out.append(hex::encode(bytes));
    out
}

/// Decimal ASCII encoding of a u64 (no leading zeros; `0` -> "0").
fun u64_to_ascii(n: u64): vector<u8> {
    if (n == 0) { return b"0" };
    let mut digits = vector::empty<u8>();
    let mut m = n;
    while (m > 0) {
        digits.push_back(((m % 10) as u8) + 48);
        m = m / 10;
    };
    // digits are least-significant-first; reverse into output.
    let mut out = vector::empty<u8>();
    let mut i = digits.length();
    while (i > 0) {
        i = i - 1;
        out.push_back(digits[i]);
    };
    out
}
