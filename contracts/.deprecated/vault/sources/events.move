module options_vault::events;

use std::type_name::TypeName;
use sui::event;

public struct VaultCreated has copy, drop {
    vault_id: ID,
    underlying_type: TypeName,
    settlement_type: TypeName,
    share_type: TypeName,
    // Active config at genesis (the consumer-facing subset; see
    // `VaultConfigApplied` for updates). Off-chain services read these so
    // they never hard-code fees/strike-band/cadence.
    mgmt_fee_bps_annual: u64,
    perf_fee_bps: u64,
    round_ms: u64,
    selling_window_ms: u64,
    min_strike_bps_over_spot: u64,
    max_strike_bps_over_spot: u64,
}

/// The active config snapshot at a finalize boundary. Emitted every finalize
/// (config is pending-then-applied, so this records what the round actually
/// ran with — robust even if `VaultCreated` was missed). Indexer upserts the
/// current snapshot from these.
public struct VaultConfigApplied has copy, drop {
    vault_id: ID,
    round: u64,
    mgmt_fee_bps_annual: u64,
    perf_fee_bps: u64,
    round_ms: u64,
    selling_window_ms: u64,
    min_strike_bps_over_spot: u64,
    max_strike_bps_over_spot: u64,
}

public struct VaultDeposit has copy, drop {
    vault_id: ID,
    depositor: address,
    /// The round the deposit participates from (receipt round).
    round: u64,
    amount: u64,
}

public struct SharesClaimed has copy, drop {
    vault_id: ID,
    owner: address,
    round: u64,
    amount: u64,
    shares: u64,
}

public struct WithdrawInitiated has copy, drop {
    vault_id: ID,
    owner: address,
    /// The round the withdrawal settles with (receipt round).
    round: u64,
    shares: u64,
}

public struct WithdrawCompleted has copy, drop {
    vault_id: ID,
    owner: address,
    round: u64,
    shares: u64,
    amount: u64,
}

public struct InstantWithdraw has copy, drop {
    vault_id: ID,
    owner: address,
    round: u64,
    amount: u64,
}

public struct VaultBucketSelected has copy, drop {
    vault_id: ID,
    round: u64,
    bucket_id: ID,
    strike: u128,
    strike_scale: u8,
    expiry_ms: u64,
    selling_ends_ms: u64,
    /// Pyth cross at selection (oracle scale).
    spot: u128,
    spot_scale: u8,
}

public struct VaultPositionRedeemed has copy, drop {
    vault_id: ID,
    round: u64,
    position_id: ID,
    underlying_returned: u64,
    settlement_returned: u64,
}

public struct VaultFeesCharged has copy, drop {
    vault_id: ID,
    round: u64,
    mgmt_fee: u64,
    perf_fee: u64,
}

public struct VaultRoundFinalized has copy, drop {
    vault_id: ID,
    /// The round that was finalized (the pps index).
    round: u64,
    pps: u128,
    aum: u64,
    /// Live shares the round's P&L accrued to (supply + queued).
    shares: u64,
    premium_collected: u64,
    premium_underlying: u64,
    withdrawals_owed: u64,
    shares_burned: u64,
    deposits_processed: u64,
    shares_minted: u64,
}

public struct VaultConfigUpdated has copy, drop {
    vault_id: ID,
    /// Configs apply at the next finalize; this is the current round.
    round: u64,
}

public struct VaultDepositsPaused has copy, drop {
    vault_id: ID,
    paused: bool,
}

/// A vault-coupled RFQ auction settled into a covered write. Mirrors the
/// options_rfq package's `RfqSettled` economics; the auction's creation
/// and bids are the generic `auction::events` types.
public struct VaultRfqSettled has copy, drop {
    auction_id: ID,
    bucket_id: ID,
    vault_id: ID,
    round: u64,
    winner: address,
    call_recipient: address,
    position_id: ID,
    amount: u64,
    gross_premium: u64,
    fee: u64,
    net_premium: u64,
    range_start: u128,
    range_end: u128,
}

/// A vault-coupled RFQ auction resolved without a write: no bids, or the
/// bucket expired/was invalidated mid-auction (escrows recovered).
public struct VaultRfqUnsold has copy, drop {
    auction_id: ID,
    bucket_id: ID,
    vault_id: ID,
    round: u64,
    amount: u64,
    reserve_premium: u64,
}

/// A swap auction filled: the winner took `settlement_filled` settlement
/// for `underlying_in` underlying, which cleared the fresh-Pyth band.
/// Carries `round` for the round-economics materializer (the realized
/// swap rate feeds the perf-fee conversion).
public struct SwapRfqSettled has copy, drop {
    swap_id: ID,
    vault_id: ID,
    round: u64,
    winner: address,
    settlement_filled: u64,
    underlying_in: u64,
}

/// A swap auction closed without converting: no bids, or the best bid
/// fell out of the Pyth band before settle (price moved). The settlement
/// returns to the vault's proceeds for re-auction.
public struct SwapRfqUnfilled has copy, drop {
    swap_id: ID,
    vault_id: ID,
    round: u64,
    amount_s: u64,
}

public(package) fun emit_vault_created(
    vault_id: ID,
    underlying_type: TypeName,
    settlement_type: TypeName,
    share_type: TypeName,
    mgmt_fee_bps_annual: u64,
    perf_fee_bps: u64,
    round_ms: u64,
    selling_window_ms: u64,
    min_strike_bps_over_spot: u64,
    max_strike_bps_over_spot: u64,
) {
    event::emit(VaultCreated {
        vault_id,
        underlying_type,
        settlement_type,
        share_type,
        mgmt_fee_bps_annual,
        perf_fee_bps,
        round_ms,
        selling_window_ms,
        min_strike_bps_over_spot,
        max_strike_bps_over_spot,
    });
}

public(package) fun emit_vault_config_applied(
    vault_id: ID,
    round: u64,
    mgmt_fee_bps_annual: u64,
    perf_fee_bps: u64,
    round_ms: u64,
    selling_window_ms: u64,
    min_strike_bps_over_spot: u64,
    max_strike_bps_over_spot: u64,
) {
    event::emit(VaultConfigApplied {
        vault_id,
        round,
        mgmt_fee_bps_annual,
        perf_fee_bps,
        round_ms,
        selling_window_ms,
        min_strike_bps_over_spot,
        max_strike_bps_over_spot,
    });
}

public(package) fun emit_vault_deposit(vault_id: ID, depositor: address, round: u64, amount: u64) {
    event::emit(VaultDeposit { vault_id, depositor, round, amount });
}

public(package) fun emit_shares_claimed(
    vault_id: ID,
    owner: address,
    round: u64,
    amount: u64,
    shares: u64,
) {
    event::emit(SharesClaimed { vault_id, owner, round, amount, shares });
}

public(package) fun emit_withdraw_initiated(vault_id: ID, owner: address, round: u64, shares: u64) {
    event::emit(WithdrawInitiated { vault_id, owner, round, shares });
}

public(package) fun emit_withdraw_completed(
    vault_id: ID,
    owner: address,
    round: u64,
    shares: u64,
    amount: u64,
) {
    event::emit(WithdrawCompleted { vault_id, owner, round, shares, amount });
}

public(package) fun emit_instant_withdraw(vault_id: ID, owner: address, round: u64, amount: u64) {
    event::emit(InstantWithdraw { vault_id, owner, round, amount });
}

public(package) fun emit_vault_bucket_selected(
    vault_id: ID,
    round: u64,
    bucket_id: ID,
    strike: u128,
    strike_scale: u8,
    expiry_ms: u64,
    selling_ends_ms: u64,
    spot: u128,
    spot_scale: u8,
) {
    event::emit(VaultBucketSelected {
        vault_id,
        round,
        bucket_id,
        strike,
        strike_scale,
        expiry_ms,
        selling_ends_ms,
        spot,
        spot_scale,
    });
}

public(package) fun emit_vault_position_redeemed(
    vault_id: ID,
    round: u64,
    position_id: ID,
    underlying_returned: u64,
    settlement_returned: u64,
) {
    event::emit(VaultPositionRedeemed {
        vault_id,
        round,
        position_id,
        underlying_returned,
        settlement_returned,
    });
}

public(package) fun emit_vault_fees_charged(vault_id: ID, round: u64, mgmt_fee: u64, perf_fee: u64) {
    event::emit(VaultFeesCharged { vault_id, round, mgmt_fee, perf_fee });
}

public(package) fun emit_vault_round_finalized(
    vault_id: ID,
    round: u64,
    pps: u128,
    aum: u64,
    shares: u64,
    premium_collected: u64,
    premium_underlying: u64,
    withdrawals_owed: u64,
    shares_burned: u64,
    deposits_processed: u64,
    shares_minted: u64,
) {
    event::emit(VaultRoundFinalized {
        vault_id,
        round,
        pps,
        aum,
        shares,
        premium_collected,
        premium_underlying,
        withdrawals_owed,
        shares_burned,
        deposits_processed,
        shares_minted,
    });
}

public(package) fun emit_vault_config_updated(vault_id: ID, round: u64) {
    event::emit(VaultConfigUpdated { vault_id, round });
}

public(package) fun emit_vault_deposits_paused(vault_id: ID, paused: bool) {
    event::emit(VaultDepositsPaused { vault_id, paused });
}

public(package) fun emit_vault_rfq_settled(
    auction_id: ID,
    bucket_id: ID,
    vault_id: ID,
    round: u64,
    winner: address,
    call_recipient: address,
    position_id: ID,
    amount: u64,
    gross_premium: u64,
    fee: u64,
    net_premium: u64,
    range_start: u128,
    range_end: u128,
) {
    event::emit(VaultRfqSettled {
        auction_id,
        bucket_id,
        vault_id,
        round,
        winner,
        call_recipient,
        position_id,
        amount,
        gross_premium,
        fee,
        net_premium,
        range_start,
        range_end,
    });
}

public(package) fun emit_vault_rfq_unsold(
    auction_id: ID,
    bucket_id: ID,
    vault_id: ID,
    round: u64,
    amount: u64,
    reserve_premium: u64,
) {
    event::emit(VaultRfqUnsold {
        auction_id,
        bucket_id,
        vault_id,
        round,
        amount,
        reserve_premium,
    });
}

public(package) fun emit_swap_rfq_settled(
    swap_id: ID,
    vault_id: ID,
    round: u64,
    winner: address,
    settlement_filled: u64,
    underlying_in: u64,
) {
    event::emit(SwapRfqSettled {
        swap_id,
        vault_id,
        round,
        winner,
        settlement_filled,
        underlying_in,
    });
}

public(package) fun emit_swap_rfq_unfilled(
    swap_id: ID,
    vault_id: ID,
    round: u64,
    amount_s: u64,
) {
    event::emit(SwapRfqUnfilled { swap_id, vault_id, round, amount_s });
}
