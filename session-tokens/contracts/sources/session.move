/// SessionCap type + the verify/open/revoke flows (spec §1.2, §1.5, §1.7) and
/// the shared spend-authorization helper used by app entrypoints (§1.6).
module siws_session::session;

use sui::address;
use sui::clock::Clock;
use sui::ed25519;

use siws_session::account::{Self, Account};
use siws_session::errors;
use siws_session::message;
use siws_session::registry::Registry;
use siws_session::siwe;

const ED25519_PUBKEY_LEN: u64 = 32;
const ED25519_SIG_LEN: u64 = 64;

/// Capability minted to the ephemeral Sui address. `store` is deliberately
/// omitted so it cannot be moved around as a freely-transferable asset; it is
/// address-owned by the temp key.
public struct SessionCap has key {
    id: UID,
    account_id: ID,
    /// must equal Account.generation or the cap is dead
    generation: u64,
    /// epoch-ms after which the cap is invalid
    expires_at_ms: u64,
    /// max cumulative spend over the cap's life
    spend_cap: u64,
    /// per-transaction max
    per_tx_cap: u64,
    /// allowlist of full `pkg::module::function` selectors this cap may call
    allowed: vector<vector<u8>>,
    /// the temp Sui address this was minted for (binding)
    holder: address,
}

// --- cap reads (for SDK status / app composition) ---

public fun account_id(cap: &SessionCap): ID { cap.account_id }
public fun generation(cap: &SessionCap): u64 { cap.generation }
public fun expires_at_ms(cap: &SessionCap): u64 { cap.expires_at_ms }
public fun spend_cap(cap: &SessionCap): u64 { cap.spend_cap }
public fun per_tx_cap(cap: &SessionCap): u64 { cap.per_tx_cap }
public fun holder(cap: &SessionCap): address { cap.holder }

/// Verify a SIWS root signature and mint a SessionCap to the temp key.
/// `T` is the coin type of the user's account. The message bytes are rebuilt
/// from THESE args (never a caller blob) and checked with ed25519.
public entry fun verify_and_open_session<T>(
    registry: &mut Registry,
    clock: &Clock,
    solana_pk: vector<u8>,
    signature: vector<u8>,
    session_key: address,
    generation: u64,
    nonce: vector<u8>,
    expires_at_ms: u64,
    spend_cap: u64,
    per_tx_cap: u64,
    allowed: vector<vector<u8>>,
    ctx: &mut TxContext,
) {
    assert!(solana_pk.length() == ED25519_PUBKEY_LEN, errors::invalid_pubkey_length());
    assert!(signature.length() == ED25519_SIG_LEN, errors::invalid_signature_length());

    // 1. freshness
    assert!(clock.timestamp_ms() < expires_at_ms, errors::expired());
    assert!(!registry.nonce_used(nonce), errors::nonce_used());

    // 2. reconstruct the signed message from checked args
    let msg = message::build_session_message(
        registry.domain(),
        registry.network(),
        solana_pk,
        session_key,
        generation,
        nonce,
        expires_at_ms,
    );

    // 3. verify ed25519
    assert!(ed25519::ed25519_verify(&signature, &solana_pk, &msg), errors::bad_sig());

    // 4. consume nonce, then find/create account + mint cap
    registry.consume_nonce(nonce);
    open_for<T>(
        registry, solana_pk, session_key, generation, expires_at_ms,
        spend_cap, per_tx_cap, allowed, ctx,
    );
}

/// EIP-4361 / Sign-In-With-Ethereum variant. The root identity is a 20-byte
/// Ethereum address recovered from a secp256k1 personal_sign signature over the
/// canonical SIWE message. The account's `owner_pk` holds the 20-byte address
/// (scheme is inferred by length: 32 = ed25519, 20 = eth).
public entry fun verify_and_open_session_eth<T>(
    registry: &mut Registry,
    clock: &Clock,
    eth_address: vector<u8>,      // 20 bytes
    signature: vector<u8>,        // 65 bytes (r || s || v in {0,1})
    session_key: address,
    generation: u64,
    nonce: vector<u8>,
    expires_at_ms: u64,
    chain_id: u64,
    issued_at: vector<u8>,
    spend_cap: u64,
    per_tx_cap: u64,
    allowed: vector<vector<u8>>,
    ctx: &mut TxContext,
) {
    assert!(eth_address.length() == siwe::eth_address_len(), errors::invalid_pubkey_length());
    assert!(signature.length() == siwe::eth_sig_len(), errors::invalid_signature_length());
    assert!(clock.timestamp_ms() < expires_at_ms, errors::expired());
    assert!(!registry.nonce_used(nonce), errors::nonce_used());

    let msg = siwe::build_message(
        registry.domain(), eth_address, session_key, generation, nonce,
        expires_at_ms, chain_id, issued_at,
    );
    let recovered = siwe::recover_eth_address(signature, msg);
    assert!(recovered == eth_address, errors::bad_sig());

    registry.consume_nonce(nonce);
    open_for<T>(
        registry, eth_address, session_key, generation, expires_at_ms,
        spend_cap, per_tx_cap, allowed, ctx,
    );
}

/// Find or create the user's Account (keyed by `identity`) and mint a cap to the
/// temp key. Shared by every root scheme.
fun open_for<T>(
    registry: &mut Registry,
    identity: vector<u8>,
    session_key: address,
    generation: u64,
    expires_at_ms: u64,
    spend_cap: u64,
    per_tx_cap: u64,
    allowed: vector<vector<u8>>,
    ctx: &mut TxContext,
) {
    let account_id = if (registry.has_account(identity)) {
        registry.account_id(identity)
    } else {
        let acct = account::new<T>(identity, ctx);
        let id = object::id(&acct);
        registry.register_account(identity, id);
        acct.share();
        id
    };
    transfer::transfer(
        SessionCap {
            id: object::new(ctx),
            account_id,
            generation,
            expires_at_ms,
            spend_cap,
            per_tx_cap,
            allowed,
            holder: session_key,
        },
        session_key,
    );
}

/// Shared enforcement used by every scoped app entrypoint (spec §1.6). Runs all
/// cap checks and records the spend; aborts on any violation. `selector` is the
/// full `pkg::module::function` byte string the entrypoint declares for itself.
public(package) fun authorize<T>(
    cap: &SessionCap,
    account: &mut Account<T>,
    clock: &Clock,
    amount: u64,
    selector: vector<u8>,
    sender: address,
) {
    // binding: cap was minted for THIS caller
    assert!(sender == cap.holder, errors::wrong_holder());
    // cap targets THIS account
    assert!(cap.account_id == object::id(account), errors::wrong_account());
    // not revoked
    assert!(cap.generation == account.generation(), errors::revoked());
    // not expired
    assert!(clock.timestamp_ms() < cap.expires_at_ms, errors::expired());
    // per-tx cap
    assert!(amount <= cap.per_tx_cap, errors::over_per_tx());
    // cumulative cap
    let prev = account.spent_of(object::id(cap));
    assert!(prev + amount <= cap.spend_cap, errors::over_total());
    // function allowlist
    assert!(is_allowed(&cap.allowed, &selector), errors::not_allowed());

    account.record_spend(object::id(cap), prev + amount);
}

/// Bump the account generation, instantly killing every outstanding cap.
/// Requires a fresh root (Solana) signature over a domain-separated
/// "revoke-v1" message (spec §1.7).
public entry fun revoke_all<T>(
    registry: &mut Registry,
    account: &mut Account<T>,
    clock: &Clock,
    solana_pk: vector<u8>,
    signature: vector<u8>,
    nonce: vector<u8>,
    expires_at_ms: u64,
) {
    assert!(account.owner_pk() == &solana_pk, errors::not_owner());
    assert!(solana_pk.length() == ED25519_PUBKEY_LEN, errors::invalid_pubkey_length());
    assert!(signature.length() == ED25519_SIG_LEN, errors::invalid_signature_length());
    assert!(clock.timestamp_ms() < expires_at_ms, errors::expired());
    assert!(!registry.nonce_used(nonce), errors::nonce_used());

    let msg = message::build_revoke_message(
        registry.domain(),
        registry.network(),
        solana_pk,
        object::id(account),
        nonce,
        expires_at_ms,
    );
    assert!(ed25519::ed25519_verify(&signature, &solana_pk, &msg), errors::bad_sig());

    registry.consume_nonce(nonce);
    account.bump_generation();
}

/// EIP-4361 revoke: bump generation after recovering the Ethereum address from
/// a personal_sign over the canonical SIWE revoke message.
public entry fun revoke_all_eth<T>(
    registry: &mut Registry,
    account: &mut Account<T>,
    clock: &Clock,
    eth_address: vector<u8>,
    signature: vector<u8>,
    nonce: vector<u8>,
    expires_at_ms: u64,
    chain_id: u64,
    issued_at: vector<u8>,
) {
    assert!(account.owner_pk() == &eth_address, errors::not_owner());
    assert!(eth_address.length() == siwe::eth_address_len(), errors::invalid_pubkey_length());
    assert!(signature.length() == siwe::eth_sig_len(), errors::invalid_signature_length());
    assert!(clock.timestamp_ms() < expires_at_ms, errors::expired());
    assert!(!registry.nonce_used(nonce), errors::nonce_used());

    let account_addr = address::from_bytes(object::id(account).to_bytes());
    let msg = siwe::build_revoke_message(
        registry.domain(), eth_address, account_addr, account.generation(), nonce,
        expires_at_ms, chain_id, issued_at,
    );
    let recovered = siwe::recover_eth_address(signature, msg);
    assert!(recovered == eth_address, errors::bad_sig());

    registry.consume_nonce(nonce);
    account.bump_generation();
}

fun is_allowed(allowed: &vector<vector<u8>>, selector: &vector<u8>): bool {
    let mut i = 0;
    while (i < allowed.length()) {
        if (allowed[i] == *selector) { return true };
        i = i + 1;
    };
    false
}

#[test_only]
public fun mint_for_testing(
    account_id: ID,
    generation: u64,
    expires_at_ms: u64,
    spend_cap: u64,
    per_tx_cap: u64,
    allowed: vector<vector<u8>>,
    holder: address,
    ctx: &mut TxContext,
): SessionCap {
    SessionCap {
        id: object::new(ctx),
        account_id,
        generation,
        expires_at_ms,
        spend_cap,
        per_tx_cap,
        allowed,
        holder,
    }
}

#[test_only]
public fun authorize_for_testing<T>(
    cap: &SessionCap,
    account: &mut Account<T>,
    clock: &Clock,
    amount: u64,
    selector: vector<u8>,
    sender: address,
) {
    authorize(cap, account, clock, amount, selector, sender)
}
