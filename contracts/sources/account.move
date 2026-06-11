module options_protocol::account;

use std::type_name;
use sui::balance::Balance;
use sui::clock::Clock;
use sui::coin::{Self, Coin};
use sui::dynamic_field as df;
use sui::dynamic_object_field as dof;

use siws_session::account::Account as SessionAccount;
use siws_session::session::{Self, SessionCap};

use options_protocol::errors;
use options_protocol::events;
use options_protocol::position::Position;

/// Signature scheme tag stored alongside the pubkey. Verified against the
/// allowed list in `assert_scheme_supported`. Tag values intentionally
/// match `quote::SCHEME_*` so we never store one and dispatch on another.
public struct Account has key {
    id: UID,
    owner: address,
    signing_scheme: u8,
    signing_pubkey: vector<u8>,
}

const SCHEME_ED25519: u8 = 0;
const SCHEME_SECP256K1: u8 = 1;
const SCHEME_SECP256R1: u8 = 2;

const ED25519_PUBKEY_LEN: u64 = 32;
const SECP256_COMPRESSED_PUBKEY_LEN: u64 = 33;

fun assert_scheme_pubkey(scheme: u8, pubkey: &vector<u8>) {
    let expected_len = if (scheme == SCHEME_ED25519) {
        ED25519_PUBKEY_LEN
    } else if (scheme == SCHEME_SECP256K1 || scheme == SCHEME_SECP256R1) {
        SECP256_COMPRESSED_PUBKEY_LEN
    } else {
        abort errors::invalid_signing_scheme()
    };
    assert!(pubkey.length() == expected_len, errors::invalid_pubkey_length());
}

public struct BalanceKey<phantom T> has copy, drop, store {}

public struct NonceKey has copy, drop, store {
    nonce: u64,
}

public fun create_account(
    signing_scheme: u8,
    signing_pubkey: vector<u8>,
    ctx: &mut TxContext,
): Account {
    assert_scheme_pubkey(signing_scheme, &signing_pubkey);
    let account = Account {
        id: object::new(ctx),
        owner: ctx.sender(),
        signing_scheme,
        signing_pubkey,
    };
    events::emit_account_created(
        object::id(&account),
        ctx.sender(),
        signing_scheme,
        signing_pubkey,
    );
    account
}

public fun create_and_share_account(
    signing_scheme: u8,
    signing_pubkey: vector<u8>,
    ctx: &mut TxContext,
) {
    let account = create_account(signing_scheme, signing_pubkey, ctx);
    transfer::share_object(account);
}

public fun deposit<T>(account: &mut Account, coin: Coin<T>) {
    let amount = coin.value();
    let key = BalanceKey<T> {};
    let bal_in = coin.into_balance();
    if (df::exists(&account.id, key)) {
        let bal: &mut Balance<T> = df::borrow_mut(&mut account.id, key);
        bal.join(bal_in);
    } else {
        df::add(&mut account.id, key, bal_in);
    };
    events::emit_account_deposit(object::id(account), type_name::with_defining_ids<T>(), amount);
}

public fun withdraw<T>(account: &mut Account, amount: u64, ctx: &mut TxContext): Coin<T> {
    assert!(ctx.sender() == account.owner, errors::not_owner());
    let coin = withdraw_internal<T>(account, amount, ctx);
    events::emit_account_withdraw(object::id(account), type_name::with_defining_ids<T>(), amount);
    coin
}

public(package) fun withdraw_internal<T>(
    account: &mut Account,
    amount: u64,
    ctx: &mut TxContext,
): Coin<T> {
    let key = BalanceKey<T> {};
    assert!(df::exists(&account.id, key), errors::insufficient_account_balance());
    let bal: &mut Balance<T> = df::borrow_mut(&mut account.id, key);
    assert!(bal.value() >= amount, errors::insufficient_account_balance());
    coin::from_balance(bal.split(amount), ctx)
}

public(package) fun deposit_balance<T>(account: &mut Account, bal_in: Balance<T>) {
    let amount = bal_in.value();
    let key = BalanceKey<T> {};
    if (df::exists(&account.id, key)) {
        let bal: &mut Balance<T> = df::borrow_mut(&mut account.id, key);
        bal.join(bal_in);
    } else {
        df::add(&mut account.id, key, bal_in);
    };
    events::emit_account_deposit(object::id(account), type_name::with_defining_ids<T>(), amount);
}

public fun set_quote_signing_key(
    account: &mut Account,
    new_scheme: u8,
    new_pubkey: vector<u8>,
    ctx: &mut TxContext,
) {
    assert!(ctx.sender() == account.owner, errors::not_owner());
    assert_scheme_pubkey(new_scheme, &new_pubkey);
    account.signing_scheme = new_scheme;
    account.signing_pubkey = new_pubkey;
    events::emit_signing_key_rotated(object::id(account), new_scheme, new_pubkey);
}

public(package) fun has_nonce(account: &Account, nonce: u64): bool {
    df::exists(&account.id, NonceKey { nonce })
}

public(package) fun consume_nonce(account: &mut Account, nonce: u64, valid_until_ms: u64) {
    df::add(&mut account.id, NonceKey { nonce }, valid_until_ms);
}

public fun prune_nonce(account: &mut Account, nonce: u64, clock: &Clock) {
    let key = NonceKey { nonce };
    if (!df::exists(&account.id, key)) {
        return
    };
    let valid_until_ms: u64 = df::remove(&mut account.id, key);
    assert!(clock.timestamp_ms() > valid_until_ms, errors::nonce_still_valid());
}

// === Session-rooted accounts (siws_session integration) ===
//
// A session user has no durable Sui address: their root identity is a
// Solana/Ethereum key and their on-chain anchor is a shared
// `siws_session::account::Account`. For those users the options `Account` is
// created WITH a session link (a dynamic field holding the session account
// id) and `owner` is set to the session account id rendered as an address —
// an address nobody holds a key for, which both keeps the plain
// sender-gated entrypoints inert for these accounts and gives the indexer a
// stable owner to attribute by. Every sender-gated function gets a
// `_with_session` duplicate gated by `session::authorize[_spend]` instead.
//
// Each `_with_session` entrypoint declares its full selector below; the SDK
// must list these in the cap's `allowed` set.

/// Dynamic-field key holding the linked session account `ID`.
public struct SessionOwnerKey has copy, drop, store {}

/// Dynamic-object-field key for a custodied `Position` (session users cannot
/// hold owned objects at a wallet address, so positions live on the account).
public struct PositionKey has copy, drop, store {
    position_id: ID,
}

/// Dynamic-field key for the index of custodied position ids (`vector<ID>`),
/// so custody can be enumerated on-chain and read in one RPC call.
public struct PositionIndexKey has copy, drop, store {}

const SEL_CREATE_ACCOUNT: vector<u8> =
    b"options_protocol::account::create_and_share_account_with_session";
const SEL_WITHDRAW: vector<u8> = b"options_protocol::account::withdraw_with_session";
const SEL_SET_QUOTE_SIGNING_KEY: vector<u8> =
    b"options_protocol::account::set_quote_signing_key_with_session";

public fun create_account_selector(): vector<u8> { SEL_CREATE_ACCOUNT }
public fun withdraw_selector(): vector<u8> { SEL_WITHDRAW }
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
    assert_scheme_pubkey(signing_scheme, &signing_pubkey);
    let mut account = Account {
        id: object::new(ctx),
        owner: session_owner_address(session_account),
        signing_scheme,
        signing_pubkey,
    };
    df::add(&mut account.id, SessionOwnerKey {}, object::id(session_account));
    events::emit_account_created(
        object::id(&account),
        account.owner,
        signing_scheme,
        signing_pubkey,
    );
    transfer::share_object(account);
}

/// Abort unless `account` is session-rooted and linked to the cap's session
/// account. Every `_with_session` entrypoint (here and in `bucket`) runs this
/// alongside `session::authorize[_spend]`, which separately binds the cap to
/// the sender and the session account.
public(package) fun assert_session_linked(account: &Account, cap: &SessionCap) {
    let key = SessionOwnerKey {};
    assert!(df::exists_(&account.id, key), errors::session_mismatch());
    let linked: &ID = df::borrow(&account.id, key);
    assert!(*linked == session::account_id(cap), errors::session_mismatch());
}

/// Linked session account id, if this is a session-rooted account.
public fun session_owner(account: &Account): Option<ID> {
    let key = SessionOwnerKey {};
    if (df::exists_(&account.id, key)) {
        option::some(*df::borrow(&account.id, key))
    } else {
        option::none()
    }
}

/// Cap-gated withdraw — the session twin of `withdraw`. Spends against the
/// cap's per-type limit for `T`.
public fun withdraw_with_session<T>(
    account: &mut Account,
    cap: &SessionCap,
    session_account: &mut SessionAccount,
    clock: &Clock,
    amount: u64,
    ctx: &mut TxContext,
): Coin<T> {
    assert_session_linked(account, cap);
    session::authorize_spend<T>(cap, session_account, clock, amount, SEL_WITHDRAW, ctx.sender());
    let coin = withdraw_internal<T>(account, amount, ctx);
    events::emit_account_withdraw(object::id(account), type_name::with_defining_ids<T>(), amount);
    coin
}

/// Cap-gated signing-key rotation — the session twin of
/// `set_quote_signing_key`.
public fun set_quote_signing_key_with_session(
    account: &mut Account,
    cap: &SessionCap,
    session_account: &SessionAccount,
    clock: &Clock,
    new_scheme: u8,
    new_pubkey: vector<u8>,
    ctx: &TxContext,
) {
    assert_session_linked(account, cap);
    session::authorize(cap, session_account, clock, SEL_SET_QUOTE_SIGNING_KEY, ctx.sender());
    assert_scheme_pubkey(new_scheme, &new_pubkey);
    account.signing_scheme = new_scheme;
    account.signing_pubkey = new_pubkey;
    events::emit_signing_key_rotated(object::id(account), new_scheme, new_pubkey);
}

// --- Position custody ---

public(package) fun store_position(account: &mut Account, position: Position) {
    let position_id = object::id(&position);
    events::emit_account_position_deposit(
        object::id(account),
        position_id,
        position.bucket_id(),
    );
    let index_key = PositionIndexKey {};
    if (df::exists_(&account.id, index_key)) {
        let index: &mut vector<ID> = df::borrow_mut(&mut account.id, index_key);
        index.push_back(position_id);
    } else {
        df::add(&mut account.id, index_key, vector[position_id]);
    };
    dof::add(&mut account.id, PositionKey { position_id }, position);
}

public(package) fun take_position(account: &mut Account, position_id: ID): Position {
    let key = PositionKey { position_id };
    assert!(dof::exists_(&account.id, key), errors::position_not_found());
    events::emit_account_position_withdraw(object::id(account), position_id);
    let index: &mut vector<ID> = df::borrow_mut(&mut account.id, PositionIndexKey {});
    let (found, i) = index.index_of(&position_id);
    assert!(found, errors::position_not_found());
    index.swap_remove(i);
    dof::remove(&mut account.id, key)
}

public fun has_position(account: &Account, position_id: ID): bool {
    dof::exists_(&account.id, PositionKey { position_id })
}

/// Ids of every custodied Position (empty for non-custody accounts).
public fun position_ids(account: &Account): vector<ID> {
    let key = PositionIndexKey {};
    if (df::exists_(&account.id, key)) {
        *df::borrow(&account.id, key)
    } else {
        vector[]
    }
}

public fun owner(account: &Account): address { account.owner }

public fun signing_pubkey(account: &Account): &vector<u8> { &account.signing_pubkey }

public fun signing_scheme(account: &Account): u8 { account.signing_scheme }

public fun balance_of<T>(account: &Account): u64 {
    let key = BalanceKey<T> {};
    if (!df::exists(&account.id, key)) {
        return 0
    };
    let bal: &Balance<T> = df::borrow(&account.id, key);
    bal.value()
}

public fun account_id(account: &Account): ID {
    object::id(account)
}
