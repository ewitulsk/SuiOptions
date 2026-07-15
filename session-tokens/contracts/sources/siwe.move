/// EIP-4361 (Sign-In With Ethereum) support: byte-exact message builder +
/// EIP-191 personal_sign reconstruction + secp256k1 ecrecover → Ethereum
/// address derivation. Mirrors `sdk/src/siwe.ts`; both are pinned against the
/// same reference vector (real signature) in their test suites.
///
/// The fixed display fields (domain/uri/statement/version) are constants on
/// both sides. The security-critical fields (registry binding, session key,
/// generation, nonce, expiry) are formatted by the contract from its own
/// state / checked args and used as the cap parameters — so a caller can't
/// misrepresent what was signed vs. what is enforced.
module siws_session::siwe;

use sui::address;
use sui::ecdsa_k1;
use sui::hash;
use sui::hex;

use siws_session::message;

/// dApp display fields — MUST match `siwe.ts`.
const DOMAIN: vector<u8> = b"siws-session.demo";
const URI: vector<u8> = b"https://siws-session.demo";
const SIGNIN_STATEMENT: vector<u8> = b"Authorize a Sui session key.";
const REVOKE_STATEMENT: vector<u8> = b"Revoke all Sui session keys.";
const WITHDRAW_STATEMENT: vector<u8> = b"Authorize a Sui withdrawal.";

/// ecrecover hash flag: 0 = KECCAK256.
const KECCAK256: u8 = 0;

const ETH_ADDRESS_LEN: u64 = 20;
const ETH_SIG_LEN: u64 = 65;

public fun eth_address_len(): u64 { ETH_ADDRESS_LEN }
public fun eth_sig_len(): u64 { ETH_SIG_LEN }

/// Sign-in message. See `sdk/src/siwe.ts::buildSiweSessionMessage`. The
/// per-type spend limits are rendered as a final `limits` resource so the
/// root signature covers them.
public fun build_message(
    registry_domain: address,
    eth_address: vector<u8>,
    session_key: address,
    generation: u64,
    nonce: vector<u8>,
    expires_at_ms: u64,
    chain_id: u64,
    issued_at: vector<u8>,
    limit_types: &vector<vector<u8>>,
    limit_per_tx: &vector<u64>,
    limit_total: &vector<u64>,
): vector<u8> {
    let mut m = build(
        SIGNIN_STATEMENT,
        registry_domain,
        eth_address,
        session_key,
        generation,
        nonce,
        expires_at_ms,
        chain_id,
        issued_at,
    );
    m.append(b"\n- siws-session://limits/");
    m.append(message::encode_limits(limit_types, limit_per_tx, limit_total));
    m
}

/// Revoke message — same shape, different statement and the `session-key`
/// resource is replaced by the account id being revoked.
public fun build_revoke_message(
    registry_domain: address,
    eth_address: vector<u8>,
    account_id: address,
    generation: u64,
    nonce: vector<u8>,
    expires_at_ms: u64,
    chain_id: u64,
    issued_at: vector<u8>,
): vector<u8> {
    build(
        REVOKE_STATEMENT,
        registry_domain,
        eth_address,
        account_id,
        generation,
        nonce,
        expires_at_ms,
        chain_id,
        issued_at,
    )
}

/// Withdrawal-authorization message (host wallet / Ethereum root). Mirrors
/// `message::build_withdraw_message` for the SIWS path and `siwe.ts`. The
/// withdrawal is pinned by the `account`, `coin-type`, `amount`, and
/// `recipient` resources so a sponsor/session key cannot redirect or resize
/// it. `coin_type` is the canonical `0x…::module::Name` form, appended raw.
public fun build_withdraw_message(
    registry_domain: address,
    eth_address: vector<u8>,
    account_id: address,
    coin_type: vector<u8>,
    amount: u64,
    recipient: address,
    nonce: vector<u8>,
    expires_at_ms: u64,
    chain_id: u64,
    issued_at: vector<u8>,
): vector<u8> {
    let mut m = vector::empty<u8>();
    m.append(DOMAIN);
    m.append(b" wants you to sign in with your Ethereum account:\n");
    m.append(b"0x");
    m.append(eip55(eth_address));
    m.append(b"\n\n");
    m.append(WITHDRAW_STATEMENT);
    m.append(b"\n\n");
    m.append(b"URI: ");
    m.append(URI);
    m.append(b"\nVersion: 1\nChain ID: ");
    m.append(u64_to_ascii(chain_id));
    m.append(b"\nNonce: ");
    m.append(hex::encode(nonce));
    m.append(b"\nIssued At: ");
    m.append(issued_at);
    m.append(b"\nResources:\n- siws-session://sui-registry/0x");
    m.append(hex::encode(address::to_bytes(registry_domain)));
    m.append(b"\n- siws-session://account/0x");
    m.append(hex::encode(address::to_bytes(account_id)));
    m.append(b"\n- siws-session://coin-type/");
    m.append(coin_type);
    m.append(b"\n- siws-session://amount/");
    m.append(u64_to_ascii(amount));
    m.append(b"\n- siws-session://recipient/0x");
    m.append(hex::encode(address::to_bytes(recipient)));
    m.append(b"\n- siws-session://expires/");
    m.append(u64_to_ascii(expires_at_ms));
    m
}

fun build(
    statement: vector<u8>,
    registry_domain: address,
    eth_address: vector<u8>,
    bound_addr: address,
    generation: u64,
    nonce: vector<u8>,
    expires_at_ms: u64,
    chain_id: u64,
    issued_at: vector<u8>,
): vector<u8> {
    let mut m = vector::empty<u8>();
    m.append(DOMAIN);
    m.append(b" wants you to sign in with your Ethereum account:\n");
    m.append(b"0x");
    m.append(eip55(eth_address));
    m.append(b"\n\n");
    m.append(statement);
    m.append(b"\n\n");
    m.append(b"URI: ");
    m.append(URI);
    m.append(b"\nVersion: 1\nChain ID: ");
    m.append(u64_to_ascii(chain_id));
    m.append(b"\nNonce: ");
    m.append(hex::encode(nonce));
    m.append(b"\nIssued At: ");
    m.append(issued_at);
    m.append(b"\nResources:\n- siws-session://sui-registry/0x");
    m.append(hex::encode(address::to_bytes(registry_domain)));
    m.append(b"\n- siws-session://session-key/0x");
    m.append(hex::encode(address::to_bytes(bound_addr)));
    m.append(b"\n- siws-session://generation/");
    m.append(u64_to_ascii(generation));
    m.append(b"\n- siws-session://expires/");
    m.append(u64_to_ascii(expires_at_ms));
    m
}

/// Recover the Ethereum address that personal_signed `message`.
public fun recover_eth_address(signature: vector<u8>, message: vector<u8>): vector<u8> {
    let prefixed = eip191(message);
    let compressed = ecdsa_k1::secp256k1_ecrecover(&signature, &prefixed, KECCAK256);
    eth_address_from_pubkey(compressed)
}

/// `\x19Ethereum Signed Message:\n<len><message>` (EIP-191 personal_sign).
fun eip191(message: vector<u8>): vector<u8> {
    let mut out = vector::empty<u8>();
    out.push_back(0x19);
    out.append(b"Ethereum Signed Message:\n");
    out.append(u64_to_ascii(message.length()));
    out.append(message);
    out
}

/// keccak256(uncompressed_pubkey[1..65])[12..32].
fun eth_address_from_pubkey(compressed: vector<u8>): vector<u8> {
    let uncompressed = ecdsa_k1::decompress_pubkey(&compressed); // 0x04 || X || Y
    let mut xy = vector::empty<u8>();
    let mut i = 1;
    while (i < 65) { xy.push_back(uncompressed[i]); i = i + 1; };
    let h = hash::keccak256(&xy);
    let mut addr = vector::empty<u8>();
    let mut j = 12;
    while (j < 32) { addr.push_back(h[j]); j = j + 1; };
    addr
}

/// EIP-55 mixed-case checksum of a 20-byte address (returns 40 ascii chars).
fun eip55(addr: vector<u8>): vector<u8> {
    let lower = hex::encode(addr); // 40 lowercase hex chars
    let h = hash::keccak256(&lower); // keccak of the ascii hex string
    let mut out = vector::empty<u8>();
    let mut i = 0;
    while (i < 40) {
        let c = lower[i];
        let byte = h[i / 2];
        let nibble = if (i % 2 == 0) { byte >> 4 } else { byte & 0x0f };
        // uppercase hex letters (a-f) whose corresponding hash nibble >= 8
        if (c >= 0x61 && c <= 0x66 && nibble >= 8) {
            out.push_back(c - 32);
        } else {
            out.push_back(c);
        };
        i = i + 1;
    };
    out
}

fun u64_to_ascii(n: u64): vector<u8> {
    if (n == 0) { return b"0" };
    let mut digits = vector::empty<u8>();
    let mut m = n;
    while (m > 0) {
        digits.push_back(((m % 10) as u8) + 48);
        m = m / 10;
    };
    let mut out = vector::empty<u8>();
    let mut i = digits.length();
    while (i > 0) {
        i = i - 1;
        out.push_back(digits[i]);
    };
    out
}
