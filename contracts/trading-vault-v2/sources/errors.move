module vault_v2::errors;

// Codes 70..113 are carried over verbatim from the v1 `trading_vault`
// package so off-chain benign-abort classification survives the cutover;
// v2-only codes continue from 120.
public fun not_curator(): u64 { 70 }
public fun wrong_vault(): u64 { 71 }
public fun vault_not_open(): u64 { 72 }
public fun vault_not_closing(): u64 { 73 }
public fun vault_not_closed(): u64 { 74 }
public fun adapter_not_allowed(): u64 { 75 }
public fun oracle_not_allowed(): u64 { 76 }
public fun deposit_asset_mismatch(): u64 { 77 }
public fun insufficient_balance(): u64 { 78 }
public fun still_locked(): u64 { 79 }
public fun curator_floor(): u64 { 80 }
public fun fee_too_high(): u64 { 81 }
public fun appraisal_incomplete(): u64 { 82 }
public fun appraisal_mismatch(): u64 { 83 }
public fun price_stale(): u64 { 84 }
public fun price_asset_mismatch(): u64 { 85 }
public fun position_missing(): u64 { 86 }
public fun already_appraised(): u64 { 87 }
public fun not_authorized(): u64 { 89 }
public fun config_invalid(): u64 { 90 }
public fun forced_session_take(): u64 { 91 }
public fun residual_assets(): u64 { 92 }
public fun positions_open(): u64 { 93 }
public fun unwind_not_ready(): u64 { 94 }
public fun stake_missing(): u64 { 95 }
public fun price_invalid(): u64 { 96 }
public fun vault_dead(): u64 { 97 }
public fun protocol_paused(): u64 { 98 }
public fun deposits_paused(): u64 { 99 }
// External-account custody.
public fun external_not_configured(): u64 { 100 }
public fun external_budget_exceeded(): u64 { 101 }
public fun external_rate_limited(): u64 { 102 }
public fun external_exposure_open(): u64 { 103 }
public fun wrong_external_oracle(): u64 { 104 }
// Attested (curator self-serve) external-account registration.
public fun external_already_set(): u64 { 105 }
public fun attested_limits_exceeded(): u64 { 106 }
public fun attestation_disabled(): u64 { 107 }
public fun bad_attestation(): u64 { 108 }
public fun oracle_not_pinned_for_asset(): u64 { 109 }
public fun asset_not_allowed(): u64 { 110 }
public fun attestation_missing(): u64 { 111 }
public fun request_missing(): u64 { 112 }
public fun quote_adapter_not_enabled(): u64 { 113 }

// ───────────────────── v2: positions and tranches ─────────────────────

/// The position belongs to a different vault than the one operated on.
public fun wrong_position_vault(): u64 { 120 }
/// The operation names a tranche the vault's capital structure does not
/// have (senior/junior on an untranched vault, or vice versa).
public fun wrong_tranche(): u64 { 121 }
/// Split/merge partners differ in vault, tranche, or capital generation.
public fun merge_incompatible(): u64 { 122 }
/// New senior issuance would leave the junior buffer below
/// `target_junior_bps` (post-deposit test, §8.4).
public fun senior_buffer_breached(): u64 { 123 }
/// The vault is in a risk-off capital state (`CoverageBreach`,
/// `Impaired`, `ResetPending`, or curator-commitment breach): deployment
/// outflows are gated per §8.4b.
public fun risk_off(): u64 { 124 }
/// The junior-reset eligibility conditions (active junior shares,
/// junior NAV == 0, total NAV < accrued senior claim) do not hold.
public fun reset_not_eligible(): u64 { 125 }
/// Reset execution attempted before the seasoning/notice deadline, or
/// with no active proposal.
public fun reset_not_ready(): u64 { 126 }
/// The recapitalization deposit does not cure the senior deficit and
/// restore the target junior buffer (§8.5.5).
public fun reset_deposit_insufficient(): u64 { 127 }
/// A reset proposal is already recorded for this generation.
public fun reset_already_proposed(): u64 { 128 }
/// The terminal settlement snapshot has not been taken yet.
public fun not_settled(): u64 { 129 }
/// The terminal settlement snapshot was already taken.
public fun already_settled(): u64 { 130 }
/// The position belongs to a wiped (pre-reset) junior generation and has
/// zero value; only `burn_wiped_position` accepts it.
public fun position_wiped(): u64 { 131 }
/// The position is NOT from a wiped generation (cleanup refused).
public fun position_not_wiped(): u64 { 132 }
/// No escrowed curator commitment position exists for the current cap.
public fun commitment_missing(): u64 { 133 }
/// Splitting more shares than the position holds (or zero / all shares
/// where a strict subset is required).
public fun invalid_split(): u64 { 134 }
/// Deposits are blocked in the current capital state (`Impaired` /
/// `ResetPending`; senior additionally in `CoverageBreach`).
public fun deposits_blocked_by_state(): u64 { 135 }
/// The vault is Closed: the queue no longer accepts requests and
/// fulfillment no longer runs — the settlement pool replaces both
/// (§8.7).
public fun queue_settled(): u64 { 136 }
