/// SessionCap type + the verify/open/revoke flows (spec §1.2, §1.5, §1.7) and
/// the authorization helpers target contracts call from their own
/// cap-gated entrypoints (§1.6).
///
/// Integration contract for a target package (see `app_example` and
/// `options_protocol`): each cap-gated entrypoint declares its own full
/// `pkg::module::function` selector constant and calls
///   - `authorize`        — for calls that move no value out of custody
///   - `authorize_spend<T>` — for calls that move `amount` of `T` out
/// Both are safe to expose publicly: every check binds to
/// `cap.holder == sender`, and a spend only ever consumes the cap's own
/// per-type budget.
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

/// Per-coin-type spend limit carried by a cap. A type with no entry cannot be
/// spent at all via `authorize_spend`.
public struct SpendLimit has copy, drop, store {
    /// canonical `0x…::module::Name` bytes (`account::canonical_type_bytes`)
    coin_type: vector<u8>,
    /// per-transaction max
    per_tx: u64,
    /// max cumulative spend over the cap's life
    total: u64,
}

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
    /// per-coin-type spend limits, covered by the root signature
    limits: vector<SpendLimit>,
    /// allowlist of full `pkg::module::function` selectors this cap may call
    allowed: vector<vector<u8>>,
    /// the temp Sui address this was minted for (binding)
    holder: address,
}

// --- cap reads (for SDK status / app composition) ---

public fun account_id(cap: &SessionCap): ID { cap.account_id }
public fun generation(cap: &SessionCap): u64 { cap.generation }
public fun expires_at_ms(cap: &SessionCap): u64 { cap.expires_at_ms }
public fun holder(cap: &SessionCap): address { cap.holder }

/// (per_tx, total) limit for `T`, or (0, 0) if the cap carries none.
public fun limit_of<T>(cap: &SessionCap): (u64, u64) {
    limit_by_type(cap, &account::canonical_type_bytes<T>())
}

/// Verify a SIWS root signature and mint a SessionCap to the temp key. The
/// message bytes are rebuilt from THESE args (never a caller blob) and checked
/// with ed25519. Limits arrive as parallel vectors (entry-fun friendly) and
/// are covered by the signature.
public entry fun verify_and_open_session(
    registry: &mut Registry,
    clock: &Clock,
    solana_pk: vector<u8>,
    signature: vector<u8>,
    session_key: address,
    generation: u64,
    nonce: vector<u8>,
    expires_at_ms: u64,
    limit_types: vector<vector<u8>>,
    limit_per_tx: vector<u64>,
    limit_total: vector<u64>,
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
        &limit_types,
        &limit_per_tx,
        &limit_total,
    );

    // 3. verify ed25519
    assert!(ed25519::ed25519_verify(&signature, &solana_pk, &msg), errors::bad_sig());

    // 4. consume nonce, then find/create account + mint cap
    registry.consume_nonce(nonce);
    open_for(
        registry, solana_pk, session_key, generation, expires_at_ms,
        zip_limits(limit_types, limit_per_tx, limit_total), allowed, ctx,
    );
}

/// EIP-4361 / Sign-In-With-Ethereum variant. The root identity is a 20-byte
/// Ethereum address recovered from a secp256k1 personal_sign signature over the
/// canonical SIWE message. The account's `owner_pk` holds the 20-byte address
/// (scheme is inferred by length: 32 = ed25519, 20 = eth).
public entry fun verify_and_open_session_eth(
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
    limit_types: vector<vector<u8>>,
    limit_per_tx: vector<u64>,
    limit_total: vector<u64>,
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
        &limit_types, &limit_per_tx, &limit_total,
    );
    let recovered = siwe::recover_eth_address(signature, msg);
    assert!(recovered == eth_address, errors::bad_sig());

    registry.consume_nonce(nonce);
    open_for(
        registry, eth_address, session_key, generation, expires_at_ms,
        zip_limits(limit_types, limit_per_tx, limit_total), allowed, ctx,
    );
}

/// Zip the entry-friendly parallel vectors into `SpendLimit`s.
fun zip_limits(
    mut types: vector<vector<u8>>,
    mut per_tx: vector<u64>,
    mut total: vector<u64>,
): vector<SpendLimit> {
    assert!(
        types.length() == per_tx.length() && types.length() == total.length(),
        errors::limits_arity_mismatch(),
    );
    let mut limits = vector::empty<SpendLimit>();
    // pop_back reverses order; order is irrelevant to lookup but keep the
    // signed order so on-chain inspection matches the message.
    types.reverse();
    per_tx.reverse();
    total.reverse();
    while (!types.is_empty()) {
        limits.push_back(SpendLimit {
            coin_type: types.pop_back(),
            per_tx: per_tx.pop_back(),
            total: total.pop_back(),
        });
    };
    limits
}

/// Find or create the user's Account (keyed by `identity`) and mint a cap to the
/// temp key. Shared by every root scheme.
fun open_for(
    registry: &mut Registry,
    identity: vector<u8>,
    session_key: address,
    generation: u64,
    expires_at_ms: u64,
    limits: vector<SpendLimit>,
    allowed: vector<vector<u8>>,
    ctx: &mut TxContext,
) {
    let account_id = if (registry.has_account(identity)) {
        registry.account_id(identity)
    } else {
        let acct = account::new(identity, ctx);
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
            limits,
            allowed,
            holder: session_key,
        },
        session_key,
    );
}

/// Cap checks shared by every scoped entrypoint that moves NO value out of
/// custody. Aborts on any violation. `selector` is the full
/// `pkg::module::function` byte string the entrypoint declares for itself.
public fun authorize(
    cap: &SessionCap,
    account: &Account,
    clock: &Clock,
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
    // function allowlist
    assert!(is_allowed(&cap.allowed, &selector), errors::not_allowed());
}

/// `authorize` + per-coin-type spend enforcement for entrypoints that move
/// `amount` of `T` out of custody. Records the spend; aborts on any violation.
public fun authorize_spend<T>(
    cap: &SessionCap,
    account: &mut Account,
    clock: &Clock,
    amount: u64,
    selector: vector<u8>,
    sender: address,
) {
    authorize(cap, account, clock, selector, sender);

    let coin_type = account::canonical_type_bytes<T>();
    let (per_tx, total) = limit_by_type(cap, &coin_type);
    // per-tx cap (a type with no limit entry has per_tx == total == 0,
    // so any non-zero spend of it aborts here)
    assert!(amount <= per_tx, errors::over_per_tx());
    // cumulative cap
    let prev = account.spent_by_type(object::id(cap), coin_type);
    assert!(prev + amount <= total, errors::over_total());

    account.record_spend(object::id(cap), coin_type, prev + amount);
}

fun limit_by_type(cap: &SessionCap, coin_type: &vector<u8>): (u64, u64) {
    let mut i = 0;
    while (i < cap.limits.length()) {
        if (&cap.limits[i].coin_type == coin_type) {
            return (cap.limits[i].per_tx, cap.limits[i].total)
        };
        i = i + 1;
    };
    (0, 0)
}

/// Verify a host-wallet (SIWS / Solana) signature authorizing a SINGLE
/// external withdrawal of `amount` of `T` from `account_id` to `recipient`,
/// and consume its nonce. Carries NO `SessionCap`: the root signature is
/// strictly stronger than any delegate, so this is the one value-exit that
/// session keys cannot drive on their own. The message is rebuilt from these
/// checked args (never a caller blob) and verified against the account's root
/// identity. Aborts on any violation; on success the caller is cleared to
/// release the coin to `recipient`.
public fun verify_withdraw_auth<T>(
    registry: &mut Registry,
    account: &Account,
    clock: &Clock,
    account_id: ID,
    amount: u64,
    recipient: address,
    signature: vector<u8>,
    nonce: vector<u8>,
    expires_at_ms: u64,
) {
    let owner_pk = *account.owner_pk();
    assert!(owner_pk.length() == ED25519_PUBKEY_LEN, errors::invalid_pubkey_length());
    assert!(signature.length() == ED25519_SIG_LEN, errors::invalid_signature_length());
    assert!(clock.timestamp_ms() < expires_at_ms, errors::expired());
    assert!(!registry.nonce_used(nonce), errors::nonce_used());

    let msg = message::build_withdraw_message(
        registry.domain(),
        registry.network(),
        owner_pk,
        account_id,
        account::canonical_type_bytes<T>(),
        amount,
        recipient,
        nonce,
        expires_at_ms,
    );
    assert!(ed25519::ed25519_verify(&signature, &owner_pk, &msg), errors::bad_sig());

    registry.consume_nonce(nonce);
}

/// EIP-4361 / Ethereum variant of `verify_withdraw_auth`. The root identity is
/// the 20-byte Ethereum address recovered from a secp256k1 personal_sign over
/// the canonical SIWE withdrawal message.
public fun verify_withdraw_auth_eth<T>(
    registry: &mut Registry,
    account: &Account,
    clock: &Clock,
    account_id: ID,
    amount: u64,
    recipient: address,
    signature: vector<u8>,
    nonce: vector<u8>,
    expires_at_ms: u64,
    chain_id: u64,
    issued_at: vector<u8>,
) {
    let owner_pk = *account.owner_pk();
    assert!(owner_pk.length() == siwe::eth_address_len(), errors::invalid_pubkey_length());
    assert!(signature.length() == siwe::eth_sig_len(), errors::invalid_signature_length());
    assert!(clock.timestamp_ms() < expires_at_ms, errors::expired());
    assert!(!registry.nonce_used(nonce), errors::nonce_used());

    let msg = siwe::build_withdraw_message(
        registry.domain(),
        owner_pk,
        address::from_bytes(account_id.to_bytes()),
        account::canonical_type_bytes<T>(),
        amount,
        recipient,
        nonce,
        expires_at_ms,
        chain_id,
        issued_at,
    );
    let recovered = siwe::recover_eth_address(signature, msg);
    assert!(recovered == owner_pk, errors::bad_sig());

    registry.consume_nonce(nonce);
}

/// Bump the account generation, instantly killing every outstanding cap.
/// Requires a fresh root (Solana) signature over a domain-separated
/// "revoke-v1" message (spec §1.7).
public entry fun revoke_all(
    registry: &mut Registry,
    account: &mut Account,
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
public entry fun revoke_all_eth(
    registry: &mut Registry,
    account: &mut Account,
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

/// Test-only: drive the non-crypto guards of the root-sig withdrawal path
/// (freshness + nonce/replay, consuming the nonce) without a real signature.
/// Options-side integration tests use this to exercise account linkage and
/// payout without pinning a signature to a non-deterministic test registry
/// domain; the real crypto path is covered by the reference-vector /
/// real-signature tests (`message`/`siwe` suites).
#[test_only]
public fun verify_withdraw_skip_sig(
    registry: &mut Registry,
    clock: &Clock,
    expires_at_ms: u64,
    nonce: vector<u8>,
) {
    assert!(clock.timestamp_ms() < expires_at_ms, errors::expired());
    assert!(!registry.nonce_used(nonce), errors::nonce_used());
    registry.consume_nonce(nonce);
}

#[test_only]
public fun new_limit_for_testing(coin_type: vector<u8>, per_tx: u64, total: u64): SpendLimit {
    SpendLimit { coin_type, per_tx, total }
}

#[test_only]
public fun mint_for_testing(
    account_id: ID,
    generation: u64,
    expires_at_ms: u64,
    limits: vector<SpendLimit>,
    allowed: vector<vector<u8>>,
    holder: address,
    ctx: &mut TxContext,
): SessionCap {
    SessionCap {
        id: object::new(ctx),
        account_id,
        generation,
        expires_at_ms,
        limits,
        allowed,
        holder,
    }
}

/// Drive `open_for` directly (skipping signature verification, which is
/// pinned separately via the serializer reference vectors) so tests can
/// exercise registry account reuse across sessions.
#[test_only]
public fun open_for_testing(
    registry: &mut Registry,
    identity: vector<u8>,
    session_key: address,
    generation: u64,
    expires_at_ms: u64,
    limits: vector<SpendLimit>,
    allowed: vector<vector<u8>>,
    ctx: &mut TxContext,
) {
    open_for(registry, identity, session_key, generation, expires_at_ms, limits, allowed, ctx);
}
