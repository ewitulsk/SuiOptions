/// Layer 2 — Lock-and-Mint Locker (Sui side), one deployment per asset
/// (bridge-spec.md §3). Built on the L1 messaging package.
///
/// - Home chain: `Vault::Escrow` holds a `Balance<T>` of the native asset.
/// - Foreign chain: `Vault::Mint` holds the wrapped coin's `TreasuryCap<T>`.
///
/// Inbound delivery uses the standard `bridge_receive` convention
/// (../relayer-dispatch-design.md): the relayer reads the Locker object's type
/// to learn `(package, module, T)` and calls `bridge_receive`, which drives
/// `inbox::receive` + `consume(&self.id)` internally — no app-specific relayer.
module locker::locker;

use sui::balance::{Self, Balance};
use sui::clock::Clock;
use sui::coin::{Self, Coin, TreasuryCap};
use sui::event;
use sui::table::{Self, Table};

use sui_bridge::envelope::SignatureEnvelope;
use sui_bridge::inbox::{Self, Inbox};
use sui_bridge::message::CrossChainMessage;
use sui_bridge::outbox::{Self, Outbox};
use sui_bridge::registry::GroupKeyRegistry;

use locker::transfer_payload;

const BYTES32_LEN: u64 = 32;

const EBadBytes32Length: u64 = 1;
const EWrongVaultMode: u64 = 2;
const EPaused: u64 = 3;
const EUnknownPeer: u64 = 4;
const EPeerMismatch: u64 = 5;
const EAssetMismatch: u64 = 6;
const ERateLimitExceeded: u64 = 7;
const EAmountHasDust: u64 = 8;
const EAdminCapMismatch: u64 = 9;
const EZeroAmount: u64 = 10;

/// Where the asset lives on this chain.
public enum Vault<phantom T> has store {
    /// Home chain: escrowed native balance.
    Escrow(Balance<T>),
    /// Foreign chain: mint/burn authority over the wrapped coin.
    Mint(TreasuryCap<T>),
}

public struct Locker<phantom T> has key {
    id: UID,
    /// 32-byte asset identifier, cross-checked against every inbound payload.
    asset_id: vector<u8>,
    vault: Vault<T>,
    /// Decimals of the local `Coin<T>`; amounts are scaled to/from the shared
    /// wire precision (`transfer_payload::wire_decimals`).
    local_decimals: u8,
    /// chain_id -> 32-byte sibling Locker address (the trusted peer per route).
    peers: Table<u32, vector<u8>>,
    paused: bool,
    // Inbound windowed rate limit (wire units). cap == 0 disables it.
    rate_limit_window_ms: u64,
    rate_limit_cap: u64,
    window_start_ms: u64,
    window_used: u64,
}

public struct LockerAdminCap has key, store {
    id: UID,
    locker_id: ID,
}

public struct LockerCreated has copy, drop {
    locker_id: ID,
    asset_id: vector<u8>,
    /// 0 = Escrow (home), 1 = Mint (foreign).
    mode: u8,
}

public struct BridgedOut has copy, drop {
    locker_id: ID,
    dst_chain_id: u32,
    nonce: u64,
    wire_amount: u64,
    recipient: vector<u8>,
}

public struct BridgedIn has copy, drop {
    locker_id: ID,
    src_chain_id: u32,
    wire_amount: u64,
    recipient: address,
}

// --- creation ---

/// Home-chain Locker: escrow vault, starts empty.
public fun create_escrow_locker<T>(
    asset_id: vector<u8>,
    local_decimals: u8,
    ctx: &mut TxContext,
) {
    create(asset_id, Vault::Escrow(balance::zero<T>()), 0, local_decimals, ctx)
}

/// Foreign-chain Locker: takes ownership of the wrapped coin's `TreasuryCap`.
public fun create_mint_locker<T>(
    treasury: TreasuryCap<T>,
    asset_id: vector<u8>,
    local_decimals: u8,
    ctx: &mut TxContext,
) {
    create(asset_id, Vault::Mint(treasury), 1, local_decimals, ctx)
}

fun create<T>(
    asset_id: vector<u8>,
    vault: Vault<T>,
    mode: u8,
    local_decimals: u8,
    ctx: &mut TxContext,
) {
    assert!(asset_id.length() == BYTES32_LEN, EBadBytes32Length);
    let locker = Locker<T> {
        id: object::new(ctx),
        asset_id,
        vault,
        local_decimals,
        peers: table::new(ctx),
        paused: false,
        rate_limit_window_ms: 0,
        rate_limit_cap: 0,
        window_start_ms: 0,
        window_used: 0,
    };
    let locker_id = object::id(&locker);
    event::emit(LockerCreated { locker_id, asset_id: locker.asset_id, mode });
    transfer::transfer(LockerAdminCap { id: object::new(ctx), locker_id }, ctx.sender());
    transfer::share_object(locker);
}

// --- governance ---

public fun set_peer<T>(
    cap: &LockerAdminCap,
    locker: &mut Locker<T>,
    chain_id: u32,
    peer_addr: vector<u8>,
) {
    assert_admin(cap, locker);
    assert!(peer_addr.length() == BYTES32_LEN, EBadBytes32Length);
    if (locker.peers.contains(chain_id)) {
        *locker.peers.borrow_mut(chain_id) = peer_addr;
    } else {
        locker.peers.add(chain_id, peer_addr);
    };
}

public fun set_paused<T>(cap: &LockerAdminCap, locker: &mut Locker<T>, paused: bool) {
    assert_admin(cap, locker);
    locker.paused = paused;
}

public fun set_rate_limit<T>(
    cap: &LockerAdminCap,
    locker: &mut Locker<T>,
    window_ms: u64,
    cap_amount: u64,
) {
    assert_admin(cap, locker);
    locker.rate_limit_window_ms = window_ms;
    locker.rate_limit_cap = cap_amount;
    locker.window_used = 0;
}

fun assert_admin<T>(cap: &LockerAdminCap, locker: &Locker<T>) {
    assert!(cap.locker_id == object::id(locker), EAdminCapMismatch);
}

// --- outbound: lock (home) / burn (foreign) ---

/// Send `coin` to `recipient` on `dst_chain_id`. Escrows (home) or burns
/// (foreign), then commits a message to the peer Locker via the Outbox.
public fun bridge_out<T>(
    locker: &mut Locker<T>,
    outbox: &mut Outbox,
    coin: Coin<T>,
    dst_chain_id: u32,
    recipient: vector<u8>,
    ): u64 {
    assert!(!locker.paused, EPaused);
    assert!(recipient.length() == BYTES32_LEN, EBadBytes32Length);
    assert!(locker.peers.contains(dst_chain_id), EUnknownPeer);

    let local_amount = coin.value();
    assert!(local_amount > 0, EZeroAmount);
    let wire_amount = to_wire(local_amount, locker.local_decimals);

    match (&mut locker.vault) {
        Vault::Escrow(bal) => { coin::put(bal, coin); },
        Vault::Mint(cap) => { coin::burn(cap, coin); },
    };

    let payload = transfer_payload::encode(
        &transfer_payload::new(locker.asset_id, wire_amount, recipient),
    );
    let peer = *locker.peers.borrow(dst_chain_id);
    let (nonce, _hash) = outbox::send(outbox, &locker.id, dst_chain_id, peer, payload);

    event::emit(BridgedOut {
        locker_id: object::id(locker),
        dst_chain_id,
        nonce,
        wire_amount,
        recipient,
    });
    nonce
}

// --- inbound: the standard delivery convention ---

/// Standard relayer entry (bridge-spec.md §3.4, dispatch-design §3.1). Verifies
/// + consumes the message via L1, then releases (home) or mints (foreign).
public fun bridge_receive<T>(
    inbox: &mut Inbox,
    keys: &GroupKeyRegistry,
    self: &mut Locker<T>,
    message: CrossChainMessage,
    envelope: SignatureEnvelope,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let delivered = inbox::receive(inbox, keys, message, envelope);
    let (src_chain_id, src_app, payload) = inbox::consume(inbox, delivered, &self.id);
    apply_inbound(self, src_chain_id, src_app, payload, clock, ctx);
}

/// App-side effects of an inbound message. Split out from `bridge_receive` so it
/// is unit-testable without standing up the L1 Inbox + a real signature.
fun apply_inbound<T>(
    self: &mut Locker<T>,
    src_chain_id: u32,
    src_app: vector<u8>,
    payload: vector<u8>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(!self.paused, EPaused);
    assert!(self.peers.contains(src_chain_id), EUnknownPeer);
    assert!(src_app == *self.peers.borrow(src_chain_id), EPeerMismatch);

    let tp = transfer_payload::decode(payload);
    assert!(transfer_payload::asset_id(&tp) == self.asset_id, EAssetMismatch);

    let wire_amount = transfer_payload::amount(&tp);
    assert!(wire_amount > 0, EZeroAmount);
    enforce_rate_limit(self, wire_amount, clock);

    let local_amount = from_wire(wire_amount, self.local_decimals);
    let recipient = sui::address::from_bytes(transfer_payload::recipient(&tp));

    match (&mut self.vault) {
        Vault::Escrow(bal) => {
            transfer::public_transfer(coin::take(bal, local_amount, ctx), recipient);
        },
        Vault::Mint(cap) => {
            transfer::public_transfer(coin::mint(cap, local_amount, ctx), recipient);
        },
    };

    event::emit(BridgedIn {
        locker_id: object::id(self),
        src_chain_id,
        wire_amount,
        recipient,
    });
}

fun enforce_rate_limit<T>(self: &mut Locker<T>, amount: u64, clock: &Clock) {
    if (self.rate_limit_cap == 0) return;
    let now = clock.timestamp_ms();
    if (now >= self.window_start_ms + self.rate_limit_window_ms) {
        self.window_start_ms = now;
        self.window_used = 0;
    };
    assert!(self.window_used + amount <= self.rate_limit_cap, ERateLimitExceeded);
    self.window_used = self.window_used + amount;
}

// --- decimals scaling (NTT trimmed-amount) ---

fun to_wire(local_amount: u64, local_decimals: u8): u64 {
    let wire = transfer_payload::wire_decimals();
    if (local_decimals >= wire) {
        let factor = pow10(local_decimals - wire);
        let amt = local_amount as u128;
        assert!(amt % factor == 0, EAmountHasDust);
        (amt / factor) as u64
    } else {
        (((local_amount as u128) * pow10(wire - local_decimals)) as u64)
    }
}

fun from_wire(wire_amount: u64, local_decimals: u8): u64 {
    let wire = transfer_payload::wire_decimals();
    if (local_decimals >= wire) {
        (((wire_amount as u128) * pow10(local_decimals - wire)) as u64)
    } else {
        let factor = pow10(wire - local_decimals);
        let amt = wire_amount as u128;
        assert!(amt % factor == 0, EAmountHasDust);
        (amt / factor) as u64
    }
}

fun pow10(n: u8): u128 {
    let mut r = 1u128;
    let mut i = 0u8;
    while (i < n) { r = r * 10; i = i + 1; };
    r
}

// --- views ---

public fun asset_id<T>(locker: &Locker<T>): vector<u8> { locker.asset_id }
public fun is_paused<T>(locker: &Locker<T>): bool { locker.paused }
public fun local_decimals<T>(locker: &Locker<T>): u8 { locker.local_decimals }

/// Escrowed balance (home chain). Aborts if this is a Mint locker.
public fun escrowed<T>(locker: &Locker<T>): u64 {
    match (&locker.vault) {
        Vault::Escrow(bal) => bal.value(),
        Vault::Mint(_) => abort EWrongVaultMode,
    }
}

#[test_only]
public fun apply_inbound_for_testing<T>(
    self: &mut Locker<T>,
    src_chain_id: u32,
    src_app: vector<u8>,
    payload: vector<u8>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    apply_inbound(self, src_chain_id, src_app, payload, clock, ctx)
}

#[test_only]
public fun fund_escrow_for_testing<T>(self: &mut Locker<T>, c: Coin<T>) {
    match (&mut self.vault) {
        Vault::Escrow(bal) => coin::put(bal, c),
        Vault::Mint(_) => abort EWrongVaultMode,
    }
}
