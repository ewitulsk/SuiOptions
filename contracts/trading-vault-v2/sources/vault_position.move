/// The tokenized vault claim (overhaul plan §2): a plain Sui object NFT
/// with `key + store`, so wallets, kiosks, multisigs, lending protocols,
/// and wrapper contracts can hold and transfer it without any
/// module-mediated path. **Transferability is unconditional for every
/// wallet-held position** — there is no controlled-transfer variant and
/// no recipient whitelist on transfer; the whitelist gates primary
/// issuance only (`create_vault` / `deposit`), never split, merge,
/// transfer, or withdrawal.
///
/// Economics travel with the object: `shares` in its tranche's supply,
/// remaining `cost_basis` (the embedded performance-fee liability a
/// secondary buyer inherits, §2.4), `locked_until_ms`, and the junior
/// `capital_generation` (§8.5) — Untranched and Senior positions carry
/// generation 0.
module vault_v2::vault_position;

use std::string;
use sui::display;
use sui::package;

use vault_v2::capital::{Self, Tranche};
use vault_v2::errors;
use vault_v2::events;

/// One-time witness for `Display` setup.
public struct VAULT_POSITION has drop {}

public struct VaultPosition has key, store {
    id: UID,
    vault_id: ID,
    tranche: Tranche,
    shares: u128,
    /// Accounting-asset smallest units; reduced pro rata on split.
    cost_basis: u64,
    locked_until_ms: u64,
    /// Junior-reset generation (§8.5). Untranched and Senior positions
    /// carry 0; Junior positions carry the generation they were minted
    /// under.
    capital_generation: u64,
}

fun init(otw: VAULT_POSITION, ctx: &mut TxContext) {
    let publisher = package::claim(otw, ctx);
    let mut d = display::new<VaultPosition>(&publisher, ctx);
    d.add(string::utf8(b"name"), string::utf8(b"Trading Vault Position"));
    d.add(
        string::utf8(b"description"),
        string::utf8(
            b"A transferable claim on a curated trading vault. Carries its own shares, cost basis (embedded performance-fee liability), lockup, tranche, and capital generation.",
        ),
    );
    d.add(string::utf8(b"vault"), string::utf8(b"{vault_id}"));
    d.add(string::utf8(b"shares"), string::utf8(b"{shares}"));
    d.add(string::utf8(b"cost_basis"), string::utf8(b"{cost_basis}"));
    d.add(string::utf8(b"locked_until_ms"), string::utf8(b"{locked_until_ms}"));
    d.add(string::utf8(b"capital_generation"), string::utf8(b"{capital_generation}"));
    d.update_version();
    transfer::public_transfer(publisher, ctx.sender());
    transfer::public_transfer(d, ctx.sender());
}

// ═══════════════════ mint / consume (vault-internal) ═══════════════════

/// Primary issuance — only `vault.move` mints.
public(package) fun mint(
    vault_id: ID,
    tranche: Tranche,
    shares: u128,
    cost_basis: u64,
    locked_until_ms: u64,
    capital_generation: u64,
    ctx: &mut TxContext,
): VaultPosition {
    let p = VaultPosition {
        id: object::new(ctx),
        vault_id,
        tranche,
        shares,
        cost_basis,
        locked_until_ms,
        capital_generation,
    };
    events::emit_position_minted(
        vault_id,
        object::id(&p),
        capital::tranche_code(&tranche),
        shares,
        cost_basis,
        locked_until_ms,
        capital_generation,
    );
    p
}

/// Consume the object into its accounting fields (withdrawal request,
/// settlement redemption, wiped-generation cleanup). Only `vault.move`.
/// Returns (position_id, tranche, shares, basis, locked_until_ms,
/// generation).
public(package) fun consume(p: VaultPosition): (ID, Tranche, u128, u64, u64, u64) {
    let VaultPosition {
        id,
        vault_id: _,
        tranche,
        shares,
        cost_basis,
        locked_until_ms,
        capital_generation,
    } = p;
    let position_id = id.to_inner();
    id.delete();
    (position_id, tranche, shares, cost_basis, locked_until_ms, capital_generation)
}

/// Credit an escrowed commitment position with fee shares (§3.5): adds
/// shares and basis without touching the lock. Only `vault.move`.
public(package) fun credit(p: &mut VaultPosition, shares: u128, basis: u64) {
    p.shares = p.shares + shares;
    p.cost_basis = p.cost_basis + basis;
}

// ═══════════════════════ split / merge (public) ═══════════════════════

/// Split `shares` out of `p` into a new position. Basis is allocated pro
/// rata with floor division — the parent keeps the remainder, so basis
/// is never created or lost (§2.3). Both objects retain the same vault,
/// tranche, generation, and lock expiry. Never whitelist-gated.
public fun split(p: &mut VaultPosition, shares: u128, ctx: &mut TxContext): VaultPosition {
    assert!(shares > 0 && shares < p.shares, errors::invalid_split());
    let child_basis =
        (((p.cost_basis as u256) * (shares as u256) / (p.shares as u256)) as u64);
    p.shares = p.shares - shares;
    p.cost_basis = p.cost_basis - child_basis;
    let child = VaultPosition {
        id: object::new(ctx),
        vault_id: p.vault_id,
        tranche: p.tranche,
        shares,
        cost_basis: child_basis,
        locked_until_ms: p.locked_until_ms,
        capital_generation: p.capital_generation,
    };
    events::emit_position_split(
        p.vault_id,
        object::id(p),
        object::id(&child),
        p.shares,
        p.cost_basis,
        shares,
        child_basis,
    );
    child
}

/// Merge `other` into `p`. Only positions with identical vault, tranche,
/// and capital generation may merge; shares and basis ADD (never
/// averaged), and the lock takes the max so merging cannot launder a
/// lockup (§2.3). Never whitelist-gated.
public fun merge(p: &mut VaultPosition, other: VaultPosition) {
    assert!(
        other.vault_id == p.vault_id
            && other.tranche == p.tranche
            && other.capital_generation == p.capital_generation,
        errors::merge_incompatible(),
    );
    let VaultPosition {
        id,
        vault_id: _,
        tranche: _,
        shares,
        cost_basis,
        locked_until_ms,
        capital_generation: _,
    } = other;
    let merged_id = id.to_inner();
    id.delete();
    p.shares = p.shares + shares;
    p.cost_basis = p.cost_basis + cost_basis;
    p.locked_until_ms = p.locked_until_ms.max(locked_until_ms);
    events::emit_position_merged(
        p.vault_id,
        object::id(p),
        merged_id,
        p.shares,
        p.cost_basis,
        p.locked_until_ms,
    );
}

// ══════════════════════════════ getters ══════════════════════════════

public fun vault_id(p: &VaultPosition): ID { p.vault_id }

public fun tranche(p: &VaultPosition): Tranche { p.tranche }

public fun shares(p: &VaultPosition): u128 { p.shares }

public fun cost_basis(p: &VaultPosition): u64 { p.cost_basis }

public fun locked_until_ms(p: &VaultPosition): u64 { p.locked_until_ms }

public fun capital_generation(p: &VaultPosition): u64 { p.capital_generation }

#[test_only]
public fun init_for_testing(ctx: &mut TxContext) {
    init(VAULT_POSITION {}, ctx)
}
