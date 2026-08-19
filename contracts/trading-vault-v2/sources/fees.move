/// Pure performance-fee arithmetic (overhaul plan §4): per-position exit
/// crystallization (§1.3 preserved into v2, §3.5 under tranching) and
/// the fee-share mint math whose senior-claim credit keeps PPS neutral.
/// No custody, no state — every function is a total function of its
/// arguments, u256 intermediates, floor division.
module vault_v2::fees;

const BPS_DENOM: u128 = 10_000;

/// Crystallize a withdrawing claim's fee (§3.5 steps 3–5):
///   profit       = max(value − basis, 0)
///   gross_fee    = profit × curator_fee_bps / 10⁴
///   protocol_cut = gross_fee × protocol_fee_bps / 10⁴  (Morpho-style)
///   curator_net  = gross_fee − protocol_cut
/// Returns (profit, gross_fee, protocol_cut, curator_net).
public fun crystallize(
    value: u64,
    basis: u64,
    curator_fee_bps: u64,
    protocol_fee_bps: u64,
): (u64, u64, u64, u64) {
    let profit = if (value > basis) { value - basis } else { 0 };
    let gross_fee = ((profit as u128) * (curator_fee_bps as u128) / BPS_DENOM) as u64;
    let protocol_cut = ((gross_fee as u128) * (protocol_fee_bps as u128) / BPS_DENOM) as u64;
    (profit, gross_fee, protocol_cut, gross_fee - protocol_cut)
}

/// Claim value at a batch-locked, offset-adjusted tranche ratio:
///   value = shares × (nav + 1) / (supply + offset)
public fun claim_value(shares: u128, nav: u128, supply: u128, offset: u128): u64 {
    (((shares as u256) * ((nav + 1) as u256) / ((supply + offset) as u256)) as u64)
}

/// Shares minted for `value` at the same locked ratio (deposit and
/// curator fee-share mint — same formula keeps PPS invariant for every
/// remaining holder):
///   shares = value × (supply + offset) / (nav + 1)
public fun shares_for_value(value: u64, nav: u128, supply: u128, offset: u128): u128 {
    (((value as u256) * ((supply + offset) as u256) / ((nav + 1) as u256)) as u128)
}
