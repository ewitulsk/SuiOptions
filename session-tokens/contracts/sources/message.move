/// Canonical message serializer (spec §1.4). The contract rebuilds the exact
/// bytes the user signed from on-chain/checked values — it NEVER trusts a
/// caller-supplied blob. The SDK reimplements `build_session_message` /
/// `build_revoke_message` byte-for-byte; the `tests/` module pins the output
/// against a fixed reference vector that the SDK test also asserts.
///
/// Format (ASCII, single `\n` separators, no trailing newline):
///
///   siws-session-v1
///   domain: 0x<32-byte registry addr, lowercase hex>
///   chain: sui:<network>
///   account: 0x<32-byte solana ed25519 pubkey, lowercase hex>
///   session_key: 0x<32-byte temp sui addr, lowercase hex>
///   generation: <decimal u64>
///   nonce: 0x<32-byte nonce, lowercase hex>
///   expires_at_ms: <decimal u64>
///
/// NOTE: we use lowercase hex (not base58) for the Solana pubkey so both sides
/// stay trivially byte-exact; the wallet renders the raw message text.
module siws_session::message;

use sui::address;
use sui::hex;

const SESSION_PREFIX: vector<u8> = b"siws-session-v1";
const REVOKE_PREFIX: vector<u8> = b"siws-session-revoke-v1";

public fun build_session_message(
    domain: address,
    network: vector<u8>,
    solana_pk: vector<u8>,
    session_key: address,
    generation: u64,
    nonce: vector<u8>,
    expires_at_ms: u64,
): vector<u8> {
    let mut m = SESSION_PREFIX;
    push_field(&mut m, b"domain: ", hex0x(address::to_bytes(domain)));
    push_field(&mut m, b"chain: ", chain_value(network));
    push_field(&mut m, b"account: ", hex0x(solana_pk));
    push_field(&mut m, b"session_key: ", hex0x(address::to_bytes(session_key)));
    push_field(&mut m, b"generation: ", u64_to_ascii(generation));
    push_field(&mut m, b"nonce: ", hex0x(nonce));
    push_field(&mut m, b"expires_at_ms: ", u64_to_ascii(expires_at_ms));
    m
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
