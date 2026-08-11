module trading_vault::errors;

// Codes continue upward from options_vault (ends at 54) so off-chain
// benign-abort classification stays collision-free across packages.
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
// External-account custody (venue capital the vault cannot hold at the
// Move level: perps margin, margin-spot, …).
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

/// The asset has an oracle PIN and the attesting witness is not it
/// (SO-335). Distinct from `oracle_not_allowed`: the adapter is
/// allowlisted protocol-wide, just not for this asset.
public fun oracle_not_pinned_for_asset(): u64 { 109 }
// Multi-asset deposits/withdrawals (SO-370).
/// The coin type is not on the vault's deposit/payout allowlist.
public fun asset_not_allowed(): u64 { 110 }
/// A non-accounting flow needs a PriceAttestation and none was supplied
/// (deposit valuation or a fulfillment batch price).
public fun attestation_missing(): u64 { 111 }
/// No pending request at that queue sequence.
public fun request_missing(): u64 { 112 }
// Direct vault escrow (SO-372).
/// The adapter witness is not on this vault's curator-managed
/// quote-session opt-in list.
public fun quote_adapter_not_enabled(): u64 { 113 }
