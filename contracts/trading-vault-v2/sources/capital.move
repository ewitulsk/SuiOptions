/// Capital-structure accounting for v2 trading vaults
/// (docs/trading-vault-overhaul-plan.md §3, §8): the immutable
/// `CapitalStructure`, the mutable `TrancheBook`, simple cumulative
/// senior-hurdle accrual (§8.2), the general three-mode waterfall
/// (§3.4a), and the capital risk-state machine (§8.4/§8.5).
///
/// Everything here is arithmetic and bookkeeping over a single appraised
/// total NAV — custody, sessions, and queues live in `vault.move`. All
/// multiply-before-divide runs in u256; floor division throughout, with
/// rounding dust favoring the vault / remaining holders, except the
/// reset minimum deposit which rounds UP (§8.5.5).
module vault_v2::capital;

use vault_v2::errors;
use vault_v2::registry::{Self, VaultProtocolConfig};

const BPS_DENOM: u128 = 10_000;
/// 365-day year, in ms — the hurdle's time basis.
const MS_PER_YEAR: u128 = 31_536_000_000;
/// Elapsed-time sanity cap per accrual calculation (§3.3): a pure
/// overflow bound, sized at two years so it can never plausibly bind in
/// operation. The keeper cadence obligation (§9.4) keeps real intervals
/// far inside it; an interval beyond the cap silently under-accrues.
const ACCRUAL_CAP_MS: u64 = 63_072_000_000;
/// Immutable protocol minimum for both the impairment seasoning period
/// and the reset advance notice (§8.5.2–3): seven days.
const RESET_SEASONING_MS: u64 = 604_800_000;

// Tranche wire codes (PTB-friendly; the enum is not constructible from a
// programmable transaction).
const TRANCHE_UNTRANCHED: u8 = 0;
const TRANCHE_SENIOR: u8 = 1;
const TRANCHE_JUNIOR: u8 = 2;

// Risk-state wire codes.
const STATE_HEALTHY: u8 = 0;
const STATE_COVERAGE_BREACH: u8 = 1;
const STATE_IMPAIRED: u8 = 2;
const STATE_RESET_PENDING: u8 = 3;

public enum Tranche has copy, drop, store {
    Untranched,
    Senior,
    Junior,
}

public enum SeniorUpside has copy, drop, store {
    PreferredOnly,
    CappedParticipating {
        residual_participation_bps: u64,
        total_return_cap_bps: u64,
    },
    UncappedParticipating {
        residual_participation_bps: u64,
    },
}

public enum CapitalStructure has copy, drop, store {
    Untranched,
    SeniorJunior {
        senior_hurdle_bps_annual: u64,
        target_junior_bps: u64,
        maintenance_junior_bps: u64,
        upside: SeniorUpside,
    },
}

/// Capital risk state (§8.4b). Vault lifecycle (Open/Closing/Closed) is
/// orthogonal and lives in `vault.move`.
public enum RiskState has copy, drop, store {
    Healthy,
    CoverageBreach,
    Impaired,
    ResetPending,
}

/// Recorded terms of a pending junior reset (§8.5). Disclosure only —
/// the binding minimum deposit is recomputed from the fresh execution
/// appraisal.
public struct JuniorResetProposal has copy, drop, store {
    old_generation: u64,
    proposed_at_ms: u64,
    /// Earliest execution: max(impaired_since + seasoning,
    /// proposed_at + notice), both at the protocol minimum.
    executable_at_ms: u64,
    recorded_nav: u128,
    recorded_senior_claim: u128,
    recorded_required_deposit: u64,
}

/// The dual share book (§3.3). An untranched vault stores its single
/// supply in `junior_shares` (its positions are `Tranche::Untranched`,
/// generation 0, and its risk state is always `Healthy`).
public struct TrancheBook has store {
    senior_shares: u128,
    junior_shares: u128,
    /// Senior capital account in accounting units: principal plus
    /// accrued hurdle, cumulative through impairment (§8.2). Reduced pro
    /// rata by shares burned at fulfillment (§3.3).
    senior_claim: u128,
    /// Senior principal without hurdle — the reference for the
    /// CappedParticipating total-return cap (§3.4a).
    senior_principal_basis: u128,
    last_accrual_ms: u64,
    active_junior_generation: u64,
    impaired_since_ms: Option<u64>,
    risk_state: RiskState,
    reset_proposal: Option<JuniorResetProposal>,
}

// ═══════════════════════════ construction ═══════════════════════════

public(package) fun new_book(now_ms: u64): TrancheBook {
    TrancheBook {
        senior_shares: 0,
        junior_shares: 0,
        senior_claim: 0,
        senior_principal_basis: 0,
        last_accrual_ms: now_ms,
        active_junior_generation: 0,
        impaired_since_ms: option::none(),
        risk_state: RiskState::Healthy,
        reset_proposal: option::none(),
    }
}

public(package) fun untranched_structure(): CapitalStructure { CapitalStructure::Untranched }

/// Build and validate a `SeniorJunior` structure from wire values
/// against the protocol bounds. Immutable after creation (§3.2).
public(package) fun senior_junior_structure(
    cfg: &VaultProtocolConfig,
    senior_hurdle_bps_annual: u64,
    target_junior_bps: u64,
    maintenance_junior_bps: u64,
    upside_code: u8,
    residual_participation_bps: u64,
    total_return_cap_bps: u64,
): CapitalStructure {
    assert!(
        senior_hurdle_bps_annual <= registry::max_senior_hurdle_bps(cfg),
        errors::config_invalid(),
    );
    assert!(
        target_junior_bps >= registry::min_target_junior_bps(cfg)
            && target_junior_bps < (BPS_DENOM as u64),
        errors::config_invalid(),
    );
    assert!(
        maintenance_junior_bps >= registry::min_maintenance_junior_bps(cfg)
            && maintenance_junior_bps <= target_junior_bps,
        errors::config_invalid(),
    );
    let upside = if (upside_code == 0) {
        assert!(residual_participation_bps == 0 && total_return_cap_bps == 0, errors::config_invalid());
        SeniorUpside::PreferredOnly
    } else if (upside_code == 1) {
        assert!(residual_participation_bps <= (BPS_DENOM as u64), errors::config_invalid());
        // The cap is on TOTAL senior return relative to principal, so
        // anything below 100% could not even repay principal.
        assert!(total_return_cap_bps >= (BPS_DENOM as u64), errors::config_invalid());
        SeniorUpside::CappedParticipating {
            residual_participation_bps,
            total_return_cap_bps,
        }
    } else if (upside_code == 2) {
        assert!(residual_participation_bps <= (BPS_DENOM as u64), errors::config_invalid());
        assert!(total_return_cap_bps == 0, errors::config_invalid());
        SeniorUpside::UncappedParticipating { residual_participation_bps }
    } else {
        abort errors::config_invalid()
    };
    CapitalStructure::SeniorJunior {
        senior_hurdle_bps_annual,
        target_junior_bps,
        maintenance_junior_bps,
        upside,
    }
}

public fun tranche_from_code(code: u8): Tranche {
    if (code == TRANCHE_UNTRANCHED) { Tranche::Untranched } else if (code == TRANCHE_SENIOR) {
        Tranche::Senior
    } else if (code == TRANCHE_JUNIOR) { Tranche::Junior } else { abort errors::wrong_tranche() }
}

public fun tranche_code(t: &Tranche): u8 {
    match (t) {
        Tranche::Untranched => TRANCHE_UNTRANCHED,
        Tranche::Senior => TRANCHE_SENIOR,
        Tranche::Junior => TRANCHE_JUNIOR,
    }
}

public fun risk_state_code(s: &RiskState): u8 {
    match (s) {
        RiskState::Healthy => STATE_HEALTHY,
        RiskState::CoverageBreach => STATE_COVERAGE_BREACH,
        RiskState::Impaired => STATE_IMPAIRED,
        RiskState::ResetPending => STATE_RESET_PENDING,
    }
}

// ═══════════════════════════ accrual ═══════════════════════════

/// Simple, continuously time-weighted, cumulative hurdle accrual
/// (§8.2 selected rule): `claim += claim × bps × elapsed / 10⁴ / year`,
/// elapsed capped at `ACCRUAL_CAP_MS`. Accrued hurdle does not itself
/// compound; it continues through impairment. Idempotent within a
/// timestamp.
public(package) fun accrue(book: &mut TrancheBook, cs: &CapitalStructure, now_ms: u64) {
    let hurdle_bps = match (cs) {
        CapitalStructure::Untranched => {
            book.last_accrual_ms = now_ms;
            return
        },
        CapitalStructure::SeniorJunior { senior_hurdle_bps_annual, .. } =>
            *senior_hurdle_bps_annual,
    };
    if (now_ms <= book.last_accrual_ms) { return };
    let elapsed = (now_ms - book.last_accrual_ms).min(ACCRUAL_CAP_MS);
    book.last_accrual_ms = now_ms;
    if (hurdle_bps == 0 || book.senior_claim == 0) { return };
    let accrual = (book.senior_claim as u256)
        * (hurdle_bps as u256)
        * (elapsed as u256)
        / (BPS_DENOM as u256)
        / (MS_PER_YEAR as u256);
    book.senior_claim = book.senior_claim + (accrual as u128);
}

/// View: the claim as it would stand after accruing to `now_ms`,
/// without mutating the book.
public fun accrued_claim_at(book: &TrancheBook, cs: &CapitalStructure, now_ms: u64): u128 {
    let hurdle_bps = match (cs) {
        CapitalStructure::Untranched => return 0,
        CapitalStructure::SeniorJunior { senior_hurdle_bps_annual, .. } =>
            *senior_hurdle_bps_annual,
    };
    if (now_ms <= book.last_accrual_ms || hurdle_bps == 0 || book.senior_claim == 0) {
        return book.senior_claim
    };
    let elapsed = (now_ms - book.last_accrual_ms).min(ACCRUAL_CAP_MS);
    let accrual = (book.senior_claim as u256)
        * (hurdle_bps as u256)
        * (elapsed as u256)
        / (BPS_DENOM as u256)
        / (MS_PER_YEAR as u256);
    book.senior_claim + (accrual as u128)
}

// ═══════════════════════════ waterfall ═══════════════════════════

/// The §3.4a general waterfall, all three upside modes. Inputs are the
/// ALREADY-ACCRUED senior claim (call `accrue` first on mutable paths)
/// and the senior principal basis. Returns `(senior_nav, junior_nav)`
/// with `senior + junior == total` exactly, in every mode.
public fun waterfall(
    cs: &CapitalStructure,
    total_nav: u128,
    accrued_senior_claim: u128,
    senior_principal_basis: u128,
): (u128, u128) {
    let upside = match (cs) {
        CapitalStructure::Untranched => return (0, total_nav),
        CapitalStructure::SeniorJunior { upside, .. } => upside,
    };
    let preferred = total_nav.min(accrued_senior_claim);
    let residual = total_nav - preferred;
    let participation = match (upside) {
        SeniorUpside::PreferredOnly => 0,
        SeniorUpside::CappedParticipating {
            residual_participation_bps,
            total_return_cap_bps,
        } => {
            let part = (residual as u256) * (*residual_participation_bps as u256)
                / (BPS_DENOM as u256);
            let cap_total = (senior_principal_basis as u256)
                * (*total_return_cap_bps as u256)
                / (BPS_DENOM as u256);
            let headroom = if (cap_total > (preferred as u256)) {
                cap_total - (preferred as u256)
            } else { 0 };
            (part.min(headroom) as u128)
        },
        SeniorUpside::UncappedParticipating { residual_participation_bps } =>
            ((residual as u256) * (*residual_participation_bps as u256)
                / (BPS_DENOM as u256)) as u128,
    };
    // Participation can never exceed residual (bps ≤ 10⁴), so senior ≤ total.
    let senior = preferred + participation;
    (senior, total_nav - senior)
}

// ═══════════════════════════ risk states ═══════════════════════════

/// Re-derive the capital risk state from a fresh waterfall (§8.4, §8.5).
/// Called from every consumed-appraisal sync in `vault.move`. Any
/// complete appraisal showing `junior_nav > 0` cancels a pending reset
/// proposal and clears `impaired_since` (§8.5.2). Returns
/// `(old_code, new_code)` so the caller can emit a transition event.
public(package) fun update_risk_state(
    book: &mut TrancheBook,
    cs: &CapitalStructure,
    total_nav: u128,
    junior_nav: u128,
    now_ms: u64,
): (u8, u8) {
    let old = risk_state_code(&book.risk_state);
    let maintenance_bps = match (cs) {
        CapitalStructure::Untranched => {
            book.risk_state = RiskState::Healthy;
            return (old, STATE_HEALTHY)
        },
        CapitalStructure::SeniorJunior { maintenance_junior_bps, .. } => *maintenance_junior_bps,
    };
    let impaired = book.senior_shares > 0 && total_nav < book.senior_claim;
    if (impaired) {
        if (book.impaired_since_ms.is_none()) {
            book.impaired_since_ms = option::some(now_ms);
        };
        book.risk_state = if (book.reset_proposal.is_some()) {
            RiskState::ResetPending
        } else {
            RiskState::Impaired
        };
    } else {
        // Recovery cancels any pending reset and clears the impairment
        // clock — time alone can never execute a wipe (§8.5.2).
        book.impaired_since_ms = option::none();
        book.reset_proposal = option::none();
        let breached = book.senior_shares > 0
            && total_nav > 0
            && (junior_nav as u256) * (BPS_DENOM as u256)
                < (maintenance_bps as u256) * (total_nav as u256);
        book.risk_state = if (breached) { RiskState::CoverageBreach } else { RiskState::Healthy };
    };
    (old, risk_state_code(&book.risk_state))
}

// ═══════════════════════ book mutations (vault-internal) ═══════════════════════

/// Deposit `value` (accounting units) minting `shares` into `tranche`.
/// Senior deposits add to both the claim and the principal basis (§3.3).
public(package) fun on_deposit(book: &mut TrancheBook, tranche: &Tranche, value: u64, shares: u128) {
    match (tranche) {
        Tranche::Senior => {
            book.senior_shares = book.senior_shares + shares;
            book.senior_claim = book.senior_claim + (value as u128);
            book.senior_principal_basis = book.senior_principal_basis + (value as u128);
        },
        _ => book.junior_shares = book.junior_shares + shares,
    }
}

/// Burn `shares` from `tranche` at fulfillment. Senior burns reduce the
/// claim pro rata by shares burned against the BATCH-LOCKED book
/// (`locked_claim`/`locked_supply`), extinguishing an exiting holder's
/// arrears rather than accreting them to remaining seniors (§3.3).
public(package) fun on_fulfill(
    book: &mut TrancheBook,
    tranche: &Tranche,
    shares: u128,
    locked_claim: u128,
    locked_senior_supply: u128,
) {
    match (tranche) {
        Tranche::Senior => {
            book.senior_shares = book.senior_shares - shares;
            if (locked_senior_supply > 0) {
                let reduction = ((locked_claim as u256) * (shares as u256)
                    / (locked_senior_supply as u256)) as u128;
                book.senior_claim = if (book.senior_claim > reduction) {
                    book.senior_claim - reduction
                } else { 0 };
                let principal_cut = ((book.senior_principal_basis as u256) * (shares as u256)
                    / (locked_senior_supply as u256)) as u128;
                book.senior_principal_basis = if (book.senior_principal_basis > principal_cut) {
                    book.senior_principal_basis - principal_cut
                } else { 0 };
            };
        },
        _ => book.junior_shares = book.junior_shares - shares,
    }
}

/// Curator fee-share mint (§3.5): same tranche as the fee was earned in.
/// A senior mint is a senior deposit for claim accounting — `curator_net`
/// credits the claim (and principal) in the same batch, keeping senior
/// PPS neutral. Exempt from the target-buffer issuance gate.
public(package) fun on_fee_mint(
    book: &mut TrancheBook,
    tranche: &Tranche,
    minted_shares: u128,
    curator_net: u64,
) {
    on_deposit(book, tranche, curator_net, minted_shares)
}

/// Record a reset proposal (§8.5.3).
public(package) fun propose_reset(
    book: &mut TrancheBook,
    now_ms: u64,
    recorded_nav: u128,
    recorded_senior_claim: u128,
    recorded_required_deposit: u64,
) {
    assert!(book.reset_proposal.is_none(), errors::reset_already_proposed());
    let impaired_since = *book.impaired_since_ms.borrow();
    let executable_at_ms =
        (impaired_since + RESET_SEASONING_MS).max(now_ms + RESET_SEASONING_MS);
    book.reset_proposal.fill(JuniorResetProposal {
        old_generation: book.active_junior_generation,
        proposed_at_ms: now_ms,
        executable_at_ms,
        recorded_nav,
        recorded_senior_claim,
        recorded_required_deposit,
    });
    book.risk_state = RiskState::ResetPending;
}

/// Generation transition at reset execution (§8.5.6): retire the old
/// junior supply, start the new generation at `minted_shares` for the
/// recapitalizer. The senior claim is NOT written down.
public(package) fun execute_reset(book: &mut TrancheBook, minted_shares: u128) {
    book.active_junior_generation = book.active_junior_generation + 1;
    book.junior_shares = minted_shares;
    book.impaired_since_ms = option::none();
    book.reset_proposal = option::none();
    book.risk_state = RiskState::Healthy;
}

/// The §8.5.5 minimum recapitalization deposit, rounded UP:
/// `D >= (C - (1 - t)·N) / (1 - t)`, from the fresh execution appraisal.
public fun min_reset_deposit(total_nav: u128, accrued_claim: u128, target_junior_bps: u64): u128 {
    let one_minus_t = (BPS_DENOM as u256) - (target_junior_bps as u256);
    let c = (accrued_claim as u256) * (BPS_DENOM as u256);
    let n = (total_nav as u256) * one_minus_t;
    if (c <= n) { return 0 };
    let num = c - n;
    // ceil(num / one_minus_t)
    (((num + one_minus_t - 1) / one_minus_t) as u128)
}

// ══════════════════════════════ getters ══════════════════════════════

public fun is_tranched(cs: &CapitalStructure): bool {
    match (cs) {
        CapitalStructure::Untranched => false,
        CapitalStructure::SeniorJunior { .. } => true,
    }
}

public fun target_junior_bps(cs: &CapitalStructure): u64 {
    match (cs) {
        CapitalStructure::Untranched => 0,
        CapitalStructure::SeniorJunior { target_junior_bps, .. } => *target_junior_bps,
    }
}

public fun maintenance_junior_bps(cs: &CapitalStructure): u64 {
    match (cs) {
        CapitalStructure::Untranched => 0,
        CapitalStructure::SeniorJunior { maintenance_junior_bps, .. } => *maintenance_junior_bps,
    }
}

public fun senior_hurdle_bps_annual(cs: &CapitalStructure): u64 {
    match (cs) {
        CapitalStructure::Untranched => 0,
        CapitalStructure::SeniorJunior { senior_hurdle_bps_annual, .. } =>
            *senior_hurdle_bps_annual,
    }
}

/// (upside_code, residual_participation_bps, total_return_cap_bps).
public fun upside_fields(cs: &CapitalStructure): (u8, u64, u64) {
    match (cs) {
        CapitalStructure::Untranched => (0, 0, 0),
        CapitalStructure::SeniorJunior { upside, .. } => match (upside) {
            SeniorUpside::PreferredOnly => (0, 0, 0),
            SeniorUpside::CappedParticipating {
                residual_participation_bps,
                total_return_cap_bps,
            } => (1, *residual_participation_bps, *total_return_cap_bps),
            SeniorUpside::UncappedParticipating { residual_participation_bps } =>
                (2, *residual_participation_bps, 0),
        },
    }
}

public fun senior_shares(book: &TrancheBook): u128 { book.senior_shares }

public fun junior_shares(book: &TrancheBook): u128 { book.junior_shares }

public fun senior_claim(book: &TrancheBook): u128 { book.senior_claim }

public fun senior_principal_basis(book: &TrancheBook): u128 { book.senior_principal_basis }

public fun last_accrual_ms(book: &TrancheBook): u64 { book.last_accrual_ms }

public fun active_junior_generation(book: &TrancheBook): u64 { book.active_junior_generation }

public fun impaired_since_ms(book: &TrancheBook): Option<u64> { book.impaired_since_ms }

public fun risk_state(book: &TrancheBook): RiskState { book.risk_state }

public fun is_healthy(book: &TrancheBook): bool { book.risk_state == RiskState::Healthy }

/// Junior-lane fulfillment and junior withdrawals-in-breach gate (§3.6):
/// the junior lane is class-blocked in every non-healthy state.
public fun is_junior_blocked(book: &TrancheBook): bool { !is_healthy(book) }

public fun has_reset_proposal(book: &TrancheBook): bool { book.reset_proposal.is_some() }

/// (old_generation, proposed_at_ms, executable_at_ms, recorded_nav,
/// recorded_senior_claim, recorded_required_deposit).
public fun reset_proposal_fields(book: &TrancheBook): (u64, u64, u64, u128, u128, u64) {
    let p = book.reset_proposal.borrow();
    (
        p.old_generation,
        p.proposed_at_ms,
        p.executable_at_ms,
        p.recorded_nav,
        p.recorded_senior_claim,
        p.recorded_required_deposit,
    )
}

public fun reset_executable_at_ms(book: &TrancheBook): u64 {
    book.reset_proposal.borrow().executable_at_ms
}

/// Outstanding shares of `tranche` (untranched supply lives in
/// `junior_shares`).
public fun supply_of(book: &TrancheBook, tranche: &Tranche): u128 {
    match (tranche) {
        Tranche::Senior => book.senior_shares,
        _ => book.junior_shares,
    }
}

public fun is_senior(t: &Tranche): bool { t == Tranche::Senior }

public fun is_junior(t: &Tranche): bool { t == Tranche::Junior }

public fun is_untranched_tranche(t: &Tranche): bool { t == Tranche::Untranched }

public fun reset_seasoning_ms(): u64 { RESET_SEASONING_MS }

public fun accrual_cap_ms(): u64 { ACCRUAL_CAP_MS }
