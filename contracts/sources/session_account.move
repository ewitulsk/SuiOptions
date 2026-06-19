/// Session-gated account layer (siws_session integration).
///
/// Everything that makes an options `Account` usable by a session-rooted
/// user (Solana / Ethereum root identity, ephemeral Sui key, sponsored gas —
/// see `session-tokens/`) lives here, cleanly separated from the plain
/// sender-gated `account` module:
///
///   - creating an Account OWNED by a session account (its id rendered as an
///     address — nobody holds a key for it, so the sender-gated entrypoints
///     stay inert) with the link held in a dynamic field;
///   - `_with_session` twins of the sender-gated functions, each declaring
///     its full selector and delegating cap checks to
///     `session::authorize[_spend]`;
///   - custody storage for objects a session user receives but cannot hold
///     at a wallet address: `Position`s and generic `key + store` objects
///     (vault receipts), kept as dynamic object fields on the account with
///     on-account id indexes so custody is enumerable in one read.
///
/// Value leaving custody is charged against the cap's per-type spend limits
/// via `session::authorize_spend`; everything else is `authorize`-gated.
module options_protocol::session_account;

use std::type_name;
use sui::clock::Clock;
use sui::dynamic_field as df;
use sui::dynamic_object_field as dof;

use siws_session::account::Account as SessionAccount;
use siws_session::registry::Registry;
use siws_session::session::{Self, SessionCap};

use options_protocol::account::{Self, Account};
use options_protocol::errors;
use options_protocol::events;
use options_protocol::position::Position;

/// Dynamic-field key holding the linked session account `ID`.
public struct SessionOwnerKey has copy, drop, store {}

/// Dynamic-object-field key for a custodied `Position`.
public struct PositionKey has copy, drop, store {
    position_id: ID,
}

/// Dynamic-field key for the index of custodied position ids (`vector<ID>`),
/// so custody can be enumerated on-chain and read in one RPC call.
public struct PositionIndexKey has copy, drop, store {}

/// Dynamic-object-field key for any other custodied object (vault
/// deposit/withdraw receipts, …). Generic custody for `key + store` objects
/// that session flows mint to the user.
public struct ObjectKey has copy, drop, store {
    object_id: ID,
}

/// Index of custodied object ids (`vector<ID>`) — consumers fetch the
/// objects to learn their types.
public struct ObjectIndexKey has copy, drop, store {}

const SEL_CREATE_ACCOUNT: vector<u8> =
    b"options_protocol::session_account::create_and_share_account_with_session";
const SEL_SET_QUOTE_SIGNING_KEY: vector<u8> =
    b"options_protocol::session_account::set_quote_signing_key_with_session";

public fun create_account_selector(): vector<u8> { SEL_CREATE_ACCOUNT }
public fun set_quote_signing_key_selector(): vector<u8> { SEL_SET_QUOTE_SIGNING_KEY }

/// The session account id rendered as an address — the session-rooted
/// account's `owner`.
public fun session_owner_address(session_account: &SessionAccount): address {
    object::id(session_account).to_address()
}

/// Create + share an options `Account` owned by the cap's session account.
public fun create_and_share_account_with_session(
    cap: &SessionCap,
    session_account: &SessionAccount,
    clock: &Clock,
    signing_scheme: u8,
    signing_pubkey: vector<u8>,
    ctx: &mut TxContext,
) {
    // `authorize` binds the cap to the sender AND to `session_account`.
    session::authorize(cap, session_account, clock, SEL_CREATE_ACCOUNT, ctx.sender());
    let mut acc = account::new_with_owner(
        session_owner_address(session_account),
        signing_scheme,
        signing_pubkey,
        ctx,
    );
    df::add(account::uid_mut(&mut acc), SessionOwnerKey {}, object::id(session_account));
    account::share_account(acc);
}

/// Abort unless `account` is session-rooted and linked to the cap's session
/// account. Every `_with_session` entrypoint (here, `session_bucket`,
/// `session_vault`) runs this alongside `session::authorize[_spend]`, which
/// separately binds the cap to the sender and the session account.
public(package) fun assert_session_linked(acc: &Account, cap: &SessionCap) {
    let key = SessionOwnerKey {};
    assert!(df::exists_(account::uid(acc), key), errors::session_mismatch());
    let linked: &ID = df::borrow(account::uid(acc), key);
    assert!(*linked == session::account_id(cap), errors::session_mismatch());
}

/// Abort unless `acc` is session-rooted and linked to exactly this
/// `session_account`. Used by the root-signature withdrawal path, which
/// carries no cap — the link is what binds `acc` to the root identity
/// (`session_account.owner_pk`) that authorizes the withdrawal.
public(package) fun assert_session_account_linked(acc: &Account, session_account: &SessionAccount) {
    let key = SessionOwnerKey {};
    assert!(df::exists_(account::uid(acc), key), errors::session_mismatch());
    let linked: &ID = df::borrow(account::uid(acc), key);
    assert!(*linked == object::id(session_account), errors::session_mismatch());
}

/// Linked session account id, if this is a session-rooted account.
public fun session_owner(acc: &Account): Option<ID> {
    let key = SessionOwnerKey {};
    if (df::exists_(account::uid(acc), key)) {
        option::some(*df::borrow(account::uid(acc), key))
    } else {
        option::none()
    }
}

/// Root-signature-gated external withdrawal — the ONLY path that moves value
/// out of a session-rooted account's custody to an arbitrary recipient.
///
/// Carries no `SessionCap`: the host wallet (Solana root) signs a message
/// binding this exact `(account, coin type, amount, recipient, nonce,
/// expiry)`, the contract rebuilds and verifies it on-chain against the
/// account's root identity, then pays the SIGNED recipient directly. The coin
/// is never returned to the PTB, so a compromised session key or sponsor can
/// neither redirect, resize, nor replay a withdrawal. In-protocol flows
/// (write/exercise/vault) move no value to arbitrary recipients and so stay
/// cap-free; this is the single re-sign chokepoint.
public entry fun withdraw_with_root_sig<T>(
    acc: &mut Account,
    session_account: &SessionAccount,
    registry: &mut Registry,
    clock: &Clock,
    amount: u64,
    recipient: address,
    signature: vector<u8>,
    nonce: vector<u8>,
    expires_at_ms: u64,
    ctx: &mut TxContext,
) {
    assert_session_account_linked(acc, session_account);
    session::verify_withdraw_auth<T>(
        registry, session_account, clock, object::id(acc),
        amount, recipient, signature, nonce, expires_at_ms,
    );
    let coin = account::withdraw_internal<T>(acc, amount, ctx);
    transfer::public_transfer(coin, recipient);
    events::emit_account_withdraw(object::id(acc), type_name::with_defining_ids<T>(), amount);
}

/// EIP-4361 / Ethereum-root variant of `withdraw_with_root_sig`.
public entry fun withdraw_with_root_sig_eth<T>(
    acc: &mut Account,
    session_account: &SessionAccount,
    registry: &mut Registry,
    clock: &Clock,
    amount: u64,
    recipient: address,
    signature: vector<u8>,
    nonce: vector<u8>,
    expires_at_ms: u64,
    chain_id: u64,
    issued_at: vector<u8>,
    ctx: &mut TxContext,
) {
    assert_session_account_linked(acc, session_account);
    session::verify_withdraw_auth_eth<T>(
        registry, session_account, clock, object::id(acc),
        amount, recipient, signature, nonce, expires_at_ms, chain_id, issued_at,
    );
    let coin = account::withdraw_internal<T>(acc, amount, ctx);
    transfer::public_transfer(coin, recipient);
    events::emit_account_withdraw(object::id(acc), type_name::with_defining_ids<T>(), amount);
}

/// Test-only twin of `withdraw_with_root_sig` that skips the real signature
/// (the crypto path is pinned separately via reference vectors). Drives the
/// account linkage, nonce/replay guard, and payout-to-recipient end to end.
#[test_only]
public fun withdraw_with_root_sig_for_testing<T>(
    acc: &mut Account,
    session_account: &SessionAccount,
    registry: &mut Registry,
    clock: &Clock,
    amount: u64,
    recipient: address,
    nonce: vector<u8>,
    expires_at_ms: u64,
    ctx: &mut TxContext,
) {
    assert_session_account_linked(acc, session_account);
    session::verify_withdraw_skip_sig(registry, clock, expires_at_ms, nonce);
    let coin = account::withdraw_internal<T>(acc, amount, ctx);
    transfer::public_transfer(coin, recipient);
    events::emit_account_withdraw(object::id(acc), type_name::with_defining_ids<T>(), amount);
}

/// Cap-gated signing-key rotation — the session twin of
/// `account::set_quote_signing_key`.
public fun set_quote_signing_key_with_session(
    acc: &mut Account,
    cap: &SessionCap,
    session_account: &SessionAccount,
    clock: &Clock,
    new_scheme: u8,
    new_pubkey: vector<u8>,
    ctx: &TxContext,
) {
    assert_session_linked(acc, cap);
    session::authorize(cap, session_account, clock, SEL_SET_QUOTE_SIGNING_KEY, ctx.sender());
    account::rotate_signing_key(acc, new_scheme, new_pubkey);
}

// --- Position custody ---

public(package) fun store_position(acc: &mut Account, position: Position) {
    let position_id = object::id(&position);
    events::emit_account_position_deposit(
        object::id(acc),
        position_id,
        position.bucket_id(),
    );
    let index_key = PositionIndexKey {};
    if (df::exists_(account::uid(acc), index_key)) {
        let index: &mut vector<ID> = df::borrow_mut(account::uid_mut(acc), index_key);
        index.push_back(position_id);
    } else {
        df::add(account::uid_mut(acc), index_key, vector[position_id]);
    };
    dof::add(account::uid_mut(acc), PositionKey { position_id }, position);
}

public(package) fun take_position(acc: &mut Account, position_id: ID): Position {
    let key = PositionKey { position_id };
    assert!(dof::exists_(account::uid(acc), key), errors::position_not_found());
    events::emit_account_position_withdraw(object::id(acc), position_id);
    let index: &mut vector<ID> = df::borrow_mut(account::uid_mut(acc), PositionIndexKey {});
    let (found, i) = index.index_of(&position_id);
    assert!(found, errors::position_not_found());
    index.swap_remove(i);
    dof::remove(account::uid_mut(acc), key)
}

public fun has_position(acc: &Account, position_id: ID): bool {
    dof::exists_(account::uid(acc), PositionKey { position_id })
}

/// Ids of every custodied Position (empty for non-custody accounts).
public fun position_ids(acc: &Account): vector<ID> {
    let key = PositionIndexKey {};
    if (df::exists_(account::uid(acc), key)) {
        *df::borrow(account::uid(acc), key)
    } else {
        vector[]
    }
}

// --- generic object custody (vault receipts, …) ---

public(package) fun store_object<O: key + store>(acc: &mut Account, obj: O) {
    let object_id = object::id(&obj);
    events::emit_account_object_deposit(
        object::id(acc),
        object_id,
        type_name::with_defining_ids<O>(),
    );
    let index_key = ObjectIndexKey {};
    if (df::exists_(account::uid(acc), index_key)) {
        let index: &mut vector<ID> = df::borrow_mut(account::uid_mut(acc), index_key);
        index.push_back(object_id);
    } else {
        df::add(account::uid_mut(acc), index_key, vector[object_id]);
    };
    dof::add(account::uid_mut(acc), ObjectKey { object_id }, obj);
}

public(package) fun take_object<O: key + store>(acc: &mut Account, object_id: ID): O {
    let key = ObjectKey { object_id };
    assert!(dof::exists_(account::uid(acc), key), errors::object_not_found());
    events::emit_account_object_withdraw(
        object::id(acc),
        object_id,
        type_name::with_defining_ids<O>(),
    );
    let index: &mut vector<ID> = df::borrow_mut(account::uid_mut(acc), ObjectIndexKey {});
    let (found, i) = index.index_of(&object_id);
    assert!(found, errors::object_not_found());
    index.swap_remove(i);
    dof::remove(account::uid_mut(acc), key)
}

public fun has_object(acc: &Account, object_id: ID): bool {
    dof::exists_(account::uid(acc), ObjectKey { object_id })
}

/// Ids of every custodied non-Position object (empty when none).
public fun object_ids(acc: &Account): vector<ID> {
    let key = ObjectIndexKey {};
    if (df::exists_(account::uid(acc), key)) {
        *df::borrow(account::uid(acc), key)
    } else {
        vector[]
    }
}
