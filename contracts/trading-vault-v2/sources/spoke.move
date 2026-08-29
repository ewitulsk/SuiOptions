/// Per-spoke hub-side state for the multichain vault
/// (docs/multichain-vault-plan.md §3). Pure data + bookkeeping; no
/// custody and no message handling — `vault.move` stores a `Spoke` per
/// bound spoke id, and `multichain.move` drives the mutations while
/// applying messages.
///
/// The hub's books here are EVENT-SOURCED from the ordered message lane
/// (deposit notices, payout receipts): `free_total` per asset is the
/// value the hub has recognized (shares minted against it), and
/// `payable` is what the hub owes spoke-side withdrawers after burning
/// their shares (§5.1). A `StateSync` never overwrites these — it is the
/// freshness proof plus a reconciliation cross-check.
module vault_v2::spoke;

use std::type_name::TypeName;
use sui::table::{Self, Table};
use sui::vec_map::{Self, VecMap};

use vault_v2::errors;

/// One asset the spoke custodies, keyed by the spoke-local asset code.
public struct SpokeAssetBook has store {
    /// Sui-side marker type used purely to price this asset via the
    /// `OracleRegistry` pin (the asset itself never exists on Sui).
    marker: TypeName,
    /// Recognized funds physically on the spoke (active + reserved),
    /// in the asset's own smallest units.
    free_total: u64,
    /// Owed to burned-share withdrawers, same units (§5.1 liability).
    payable: u64,
}

/// Ledger key: one holding per (user, tranche).
public struct HoldingKey has copy, drop, store {
    user: address,
    tranche: u8,
}

/// A spoke depositor's hub-side claim. Same share/basis semantics as a
/// wallet `VaultPosition`; `locked_until_ms` is hub-clock.
public struct Holding has store {
    shares: u128,
    basis: u64,
    locked_until_ms: u64,
    /// Junior generation at mint (0 for senior/untranched) — wiped
    /// generations are worthless, mirroring `VaultPosition`.
    capital_generation: u64,
}

public struct Spoke has store {
    chain_id: u64,
    /// The spoke vault contract, as 32 wire bytes (EVM left-padded).
    vault_address: address,
    /// Bound transport witness type; the only endpoint whose deliveries
    /// this spoke accepts (§2.2: one active endpoint per spoke).
    endpoint: TypeName,
    /// Payouts are denominated in this asset code.
    payout_asset: u8,
    assets: VecMap<u8, SpokeAssetBook>,
    holdings: Table<HoldingKey, Holding>,
    holdings_count: u64,
    /// Last APPLIED spoke→hub sequence (0 = none; first message is 1).
    inbound_seq: u64,
    /// Last EMITTED hub→spoke sequence.
    outbound_seq: u64,
    /// Spoke-clock timestamp of the last applied StateSync; appraisal
    /// legs require it fresher than `max_sync_age_ms`.
    last_sync_ms: u64,
    max_sync_age_ms: u64,
    /// Hub refuses to ACK a deposit notice older than this (spoke-side
    /// reclaim opens after its own, strictly larger, timeout).
    ack_deadline_ms: u64,
    /// Latest reported fee-pot balance (native units; dashboards only).
    fee_pot_balance: u128,
    /// Hub-valued deployed-integration equity (0 until an integration
    /// with a valuation adapter is registered).
    integration_value: u64,
    /// The curator's address ON THE SPOKE CHAIN (32 wire bytes),
    /// propagated via ConfigSync — never a spoke-local role (§6.1).
    curator_address: address,
    /// Numeric endpoint id the spoke maps to a local endpoint contract;
    /// ConfigSync carries it so endpoint switches propagate (§2.2).
    endpoint_code: u8,
    /// Commitment to the spoke's registered integration set (§6);
    /// zero = no integrations.
    integrations_root: address,
}

// ═══════════════════════════ construction ═══════════════════════════

public(package) fun new(
    chain_id: u64,
    vault_address: address,
    endpoint: TypeName,
    payout_asset: u8,
    payout_marker: TypeName,
    max_sync_age_ms: u64,
    ack_deadline_ms: u64,
    now_spoke_ms: u64,
    curator_address: address,
    endpoint_code: u8,
    ctx: &mut TxContext,
): Spoke {
    let mut assets = vec_map::empty();
    assets.insert(
        payout_asset,
        SpokeAssetBook { marker: payout_marker, free_total: 0, payable: 0 },
    );
    Spoke {
        chain_id,
        vault_address,
        endpoint,
        payout_asset,
        assets,
        holdings: table::new(ctx),
        holdings_count: 0,
        inbound_seq: 0,
        outbound_seq: 0,
        last_sync_ms: now_spoke_ms,
        max_sync_age_ms,
        ack_deadline_ms,
        fee_pot_balance: 0,
        integration_value: 0,
        curator_address,
        endpoint_code,
        integrations_root: @0x0,
    }
}

public(package) fun set_curator_address(spoke: &mut Spoke, curator: address) {
    spoke.curator_address = curator;
}

public(package) fun set_integrations_root(spoke: &mut Spoke, root: address) {
    spoke.integrations_root = root;
}

public(package) fun add_asset(spoke: &mut Spoke, code: u8, marker: TypeName) {
    assert!(!spoke.assets.contains(&code), errors::config_invalid());
    spoke.assets.insert(code, SpokeAssetBook { marker, free_total: 0, payable: 0 });
}

/// Destroy a fully drained spoke (unbind / pre-close guard).
public(package) fun destroy_drained(spoke: Spoke) {
    let Spoke {
        chain_id: _,
        vault_address: _,
        endpoint: _,
        payout_asset: _,
        mut assets,
        holdings,
        holdings_count,
        inbound_seq: _,
        outbound_seq: _,
        last_sync_ms: _,
        max_sync_age_ms: _,
        ack_deadline_ms: _,
        fee_pot_balance: _,
        integration_value,
        curator_address: _,
        endpoint_code: _,
        integrations_root: _,
    } = spoke;
    assert!(holdings_count == 0 && integration_value == 0, errors::spoke_not_drained());
    holdings.destroy_empty();
    while (!assets.is_empty()) {
        let (_, book) = assets.pop();
        let SpokeAssetBook { marker: _, free_total, payable } = book;
        assert!(free_total == 0 && payable == 0, errors::spoke_not_drained());
    };
    assets.destroy_empty();
}

// ══════════════════════════ sequencing ══════════════════════════

/// Apply the next inbound sequence number; aborts unless exactly +1 —
/// per-lane ordering and replay protection in one check.
public(package) fun apply_inbound_seq(spoke: &mut Spoke, seq: u64) {
    assert!(seq == spoke.inbound_seq + 1, errors::bad_sequence());
    spoke.inbound_seq = seq;
}

public(package) fun next_outbound_seq(spoke: &mut Spoke): u64 {
    spoke.outbound_seq = spoke.outbound_seq + 1;
    spoke.outbound_seq
}

// ══════════════════════════ books ══════════════════════════

public(package) fun credit_free(spoke: &mut Spoke, code: u8, amount: u64) {
    let book = spoke.assets.get_mut(&code);
    book.free_total = book.free_total + amount;
}

/// Payout receipt: the spoke physically paid `amount`, extinguishing
/// that much payable and the funds backing it. Clamps (with the clamped
/// amount returned for alarming) rather than aborting — a receipt is a
/// statement of fact from our own spoke contract, and the lane must
/// keep moving even if books drifted.
public(package) fun settle_payout(spoke: &mut Spoke, code: u8, amount: u64): u64 {
    let book = spoke.assets.get_mut(&code);
    let pay = amount.min(book.payable).min(book.free_total);
    book.payable = book.payable - pay;
    book.free_total = book.free_total - pay;
    amount - pay
}

public(package) fun book_payable(spoke: &mut Spoke, code: u8, amount: u64) {
    let book = spoke.assets.get_mut(&code);
    book.payable = book.payable + amount;
}

public(package) fun record_sync(
    spoke: &mut Spoke,
    ts_ms: u64,
    fee_pot_balance: u128,
) {
    // Spoke clocks must move forward; a rewind is ignored for freshness.
    if (ts_ms > spoke.last_sync_ms) { spoke.last_sync_ms = ts_ms };
    spoke.fee_pot_balance = fee_pot_balance;
}

// ══════════════════════════ holdings ══════════════════════════

public(package) fun credit_holding(
    spoke: &mut Spoke,
    user: address,
    tranche: u8,
    shares: u128,
    basis: u64,
    locked_until_ms: u64,
    generation: u64,
) {
    let key = HoldingKey { user, tranche };
    if (spoke.holdings.contains(key)) {
        let h = spoke.holdings.borrow_mut(key);
        if (h.capital_generation < generation) {
            // Wiped junior claim: the old shares are permanently
            // worthless (§8.5); restart the holding fresh.
            h.shares = shares;
            h.basis = basis;
            h.capital_generation = generation;
        } else {
            h.shares = h.shares + shares;
            h.basis = h.basis + basis;
        };
        // A top-up extends the lockup for the merged claim.
        h.locked_until_ms = h.locked_until_ms.max(locked_until_ms);
    } else {
        spoke.holdings.add(
            key,
            Holding { shares, basis, locked_until_ms, capital_generation: generation },
        );
        spoke.holdings_count = spoke.holdings_count + 1;
    }
}

public(package) fun has_holding(spoke: &Spoke, user: address, tranche: u8): bool {
    spoke.holdings.contains(HoldingKey { user, tranche })
}

/// (shares, basis, locked_until_ms, capital_generation)
public(package) fun holding_fields(
    spoke: &Spoke,
    user: address,
    tranche: u8,
): (u128, u64, u64, u64) {
    let h = spoke.holdings.borrow(HoldingKey { user, tranche });
    (h.shares, h.basis, h.locked_until_ms, h.capital_generation)
}

/// Debit `shares` (and the pro-rata slice of basis) from a holding,
/// removing it when empty. Returns the basis debited.
public(package) fun debit_holding(
    spoke: &mut Spoke,
    user: address,
    tranche: u8,
    shares: u128,
): u64 {
    let key = HoldingKey { user, tranche };
    let (empty, basis_out) = {
        let h = spoke.holdings.borrow_mut(key);
        assert!(shares > 0 && shares <= h.shares, errors::invalid_split());
        let basis_out = if (shares == h.shares) { h.basis } else {
            (((h.basis as u256) * (shares as u256) / (h.shares as u256)) as u64)
        };
        h.shares = h.shares - shares;
        h.basis = h.basis - basis_out;
        (h.shares == 0, basis_out)
    };
    if (empty) {
        let Holding { shares: _, basis: _, locked_until_ms: _, capital_generation: _ } =
            spoke.holdings.remove(key);
        spoke.holdings_count = spoke.holdings_count - 1;
    };
    basis_out
}

/// Drop a wiped-generation holding entirely (worthless claim, §8.5).
public(package) fun burn_wiped_holding(spoke: &mut Spoke, user: address, tranche: u8): u128 {
    let key = HoldingKey { user, tranche };
    let Holding { shares, basis: _, locked_until_ms: _, capital_generation: _ } =
        spoke.holdings.remove(key);
    spoke.holdings_count = spoke.holdings_count - 1;
    shares
}

// ══════════════════════════ getters ══════════════════════════

public fun chain_id(spoke: &Spoke): u64 { spoke.chain_id }

public fun vault_address(spoke: &Spoke): address { spoke.vault_address }

public fun endpoint(spoke: &Spoke): TypeName { spoke.endpoint }

public fun payout_asset(spoke: &Spoke): u8 { spoke.payout_asset }

public fun has_asset(spoke: &Spoke, code: u8): bool { spoke.assets.contains(&code) }

public fun asset_marker(spoke: &Spoke, code: u8): TypeName { spoke.assets.get(&code).marker }

public fun free_total(spoke: &Spoke, code: u8): u64 { spoke.assets.get(&code).free_total }

public fun payable(spoke: &Spoke, code: u8): u64 { spoke.assets.get(&code).payable }

public fun asset_codes(spoke: &Spoke): vector<u8> {
    let mut codes = vector[];
    let mut i = 0;
    while (i < spoke.assets.length()) {
        let (code, _) = spoke.assets.get_entry_by_idx(i);
        codes.push_back(*code);
        i = i + 1;
    };
    codes
}

public fun inbound_seq(spoke: &Spoke): u64 { spoke.inbound_seq }

public fun outbound_seq(spoke: &Spoke): u64 { spoke.outbound_seq }

public fun last_sync_ms(spoke: &Spoke): u64 { spoke.last_sync_ms }

public fun max_sync_age_ms(spoke: &Spoke): u64 { spoke.max_sync_age_ms }

public fun ack_deadline_ms(spoke: &Spoke): u64 { spoke.ack_deadline_ms }

public fun fee_pot_balance(spoke: &Spoke): u128 { spoke.fee_pot_balance }

public fun integration_value(spoke: &Spoke): u64 { spoke.integration_value }

public fun curator_address(spoke: &Spoke): address { spoke.curator_address }

public fun endpoint_code(spoke: &Spoke): u8 { spoke.endpoint_code }

public fun integrations_root(spoke: &Spoke): address { spoke.integrations_root }

public fun holdings_count(spoke: &Spoke): u64 { spoke.holdings_count }

/// True when nothing of value remains: safe to unbind / close over.
public fun is_drained(spoke: &Spoke): bool {
    if (spoke.holdings_count != 0 || spoke.integration_value != 0) { return false };
    let mut i = 0;
    while (i < spoke.assets.length()) {
        let (_, book) = spoke.assets.get_entry_by_idx(i);
        if (book.free_total != 0 || book.payable != 0) { return false };
        i = i + 1;
    };
    true
}
