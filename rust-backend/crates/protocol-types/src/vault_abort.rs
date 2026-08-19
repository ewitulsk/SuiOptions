//! Trading-vault abort codes — the shared off-chain mirror of
//! `contracts/trading-vault-v2/sources/errors.move`.
//!
//! Codes 70..113 are carried over verbatim from the v1 `trading_vault`
//! package so benign-abort classification survives the cutover; v2-only
//! codes continue from 120. Keeper / orderbook / mm-bot MUST consume these
//! constants instead of re-declaring the numbers (WS-0.3). Names are the
//! UPPER_SNAKE form of the Move error-fn names.

// ─── v1 carry-over (70..113) ───

pub const NOT_CURATOR: u64 = 70;
pub const WRONG_VAULT: u64 = 71;
pub const VAULT_NOT_OPEN: u64 = 72;
pub const VAULT_NOT_CLOSING: u64 = 73;
pub const VAULT_NOT_CLOSED: u64 = 74;
pub const ADAPTER_NOT_ALLOWED: u64 = 75;
pub const ORACLE_NOT_ALLOWED: u64 = 76;
pub const DEPOSIT_ASSET_MISMATCH: u64 = 77;
pub const INSUFFICIENT_BALANCE: u64 = 78;
pub const STILL_LOCKED: u64 = 79;
pub const CURATOR_FLOOR: u64 = 80;
pub const FEE_TOO_HIGH: u64 = 81;
pub const APPRAISAL_INCOMPLETE: u64 = 82;
pub const APPRAISAL_MISMATCH: u64 = 83;
pub const PRICE_STALE: u64 = 84;
pub const PRICE_ASSET_MISMATCH: u64 = 85;
pub const POSITION_MISSING: u64 = 86;
pub const ALREADY_APPRAISED: u64 = 87;
pub const NOT_AUTHORIZED: u64 = 89;
pub const CONFIG_INVALID: u64 = 90;
pub const FORCED_SESSION_TAKE: u64 = 91;
pub const RESIDUAL_ASSETS: u64 = 92;
pub const POSITIONS_OPEN: u64 = 93;
pub const UNWIND_NOT_READY: u64 = 94;
pub const STAKE_MISSING: u64 = 95;
pub const PRICE_INVALID: u64 = 96;
pub const VAULT_DEAD: u64 = 97;
pub const PROTOCOL_PAUSED: u64 = 98;
pub const DEPOSITS_PAUSED: u64 = 99;
// External-account custody.
pub const EXTERNAL_NOT_CONFIGURED: u64 = 100;
pub const EXTERNAL_BUDGET_EXCEEDED: u64 = 101;
pub const EXTERNAL_RATE_LIMITED: u64 = 102;
pub const EXTERNAL_EXPOSURE_OPEN: u64 = 103;
pub const WRONG_EXTERNAL_ORACLE: u64 = 104;
// Attested (curator self-serve) external-account registration.
pub const EXTERNAL_ALREADY_SET: u64 = 105;
pub const ATTESTED_LIMITS_EXCEEDED: u64 = 106;
pub const ATTESTATION_DISABLED: u64 = 107;
pub const BAD_ATTESTATION: u64 = 108;
pub const ORACLE_NOT_PINNED_FOR_ASSET: u64 = 109;
pub const ASSET_NOT_ALLOWED: u64 = 110;
pub const ATTESTATION_MISSING: u64 = 111;
pub const REQUEST_MISSING: u64 = 112;
pub const QUOTE_ADAPTER_NOT_ENABLED: u64 = 113;

// ─── v2: positions and tranches (120..136) ───

/// The position belongs to a different vault than the one operated on.
pub const WRONG_POSITION_VAULT: u64 = 120;
/// The operation names a tranche the vault's capital structure lacks.
pub const WRONG_TRANCHE: u64 = 121;
/// Split/merge partners differ in vault, tranche, or capital generation.
pub const MERGE_INCOMPATIBLE: u64 = 122;
/// New senior issuance would leave the junior buffer below target.
pub const SENIOR_BUFFER_BREACHED: u64 = 123;
/// The vault is in a risk-off capital state: deployment outflows gated.
pub const RISK_OFF: u64 = 124;
/// Junior-reset eligibility conditions do not hold.
pub const RESET_NOT_ELIGIBLE: u64 = 125;
/// Reset execution before the notice deadline, or no active proposal.
pub const RESET_NOT_READY: u64 = 126;
/// The recapitalization deposit does not cure the senior deficit.
pub const RESET_DEPOSIT_INSUFFICIENT: u64 = 127;
/// A reset proposal is already recorded for this generation.
pub const RESET_ALREADY_PROPOSED: u64 = 128;
/// The terminal settlement snapshot has not been taken yet.
pub const NOT_SETTLED: u64 = 129;
/// The terminal settlement snapshot was already taken.
pub const ALREADY_SETTLED: u64 = 130;
/// The position is from a wiped junior generation (zero value).
pub const POSITION_WIPED: u64 = 131;
/// The position is NOT from a wiped generation (cleanup refused).
pub const POSITION_NOT_WIPED: u64 = 132;
/// No escrowed curator commitment position for the current cap.
pub const COMMITMENT_MISSING: u64 = 133;
/// Splitting more shares than held (or zero / a non-strict subset).
pub const INVALID_SPLIT: u64 = 134;
/// Deposits blocked in the current capital state.
pub const DEPOSITS_BLOCKED_BY_STATE: u64 = 135;
/// The vault is Closed: queue and fulfillment are replaced by the
/// settlement pool.
pub const QUEUE_SETTLED: u64 = 136;
