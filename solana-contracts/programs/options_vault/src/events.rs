use anchor_lang::prelude::*;

// Ports the Vault* events from events.move.

#[event]
pub struct VaultCreated {
    pub vault: Pubkey,
    pub underlying_mint: Pubkey,
    pub settlement_mint: Pubkey,
    pub share_mint: Pubkey,
    pub mgmt_fee_bps_annual: u64,
    pub perf_fee_bps: u64,
    pub round_ms: u64,
    pub selling_window_ms: u64,
    pub min_strike_bps_over_spot: u64,
    pub max_strike_bps_over_spot: u64,
}

#[event]
pub struct VaultConfigUpdated {
    pub vault: Pubkey,
    pub round: u64,
}

#[event]
pub struct VaultConfigApplied {
    pub vault: Pubkey,
    pub round: u64,
    pub mgmt_fee_bps_annual: u64,
    pub perf_fee_bps: u64,
    pub round_ms: u64,
    pub selling_window_ms: u64,
    pub min_strike_bps_over_spot: u64,
    pub max_strike_bps_over_spot: u64,
}

#[event]
pub struct VaultDepositsPaused {
    pub vault: Pubkey,
    pub paused: bool,
}

#[event]
pub struct VaultDeposit {
    pub vault: Pubkey,
    pub depositor: Pubkey,
    pub round: u64,
    pub amount: u64,
}

#[event]
pub struct SharesClaimed {
    pub vault: Pubkey,
    pub claimer: Pubkey,
    pub round: u64,
    pub amount: u64,
    pub shares: u64,
}

#[event]
pub struct WithdrawInitiated {
    pub vault: Pubkey,
    pub withdrawer: Pubkey,
    pub round: u64,
    pub shares: u64,
}

#[event]
pub struct WithdrawCompleted {
    pub vault: Pubkey,
    pub withdrawer: Pubkey,
    pub round: u64,
    pub shares: u64,
    pub amount: u64,
}

#[event]
pub struct InstantWithdraw {
    pub vault: Pubkey,
    pub withdrawer: Pubkey,
    pub round: u64,
    pub amount: u64,
}

#[event]
pub struct VaultBucketSelected {
    pub vault: Pubkey,
    pub round: u64,
    pub bucket: Pubkey,
    pub strike: u128,
    pub strike_scale: u8,
    pub expiry_ms: u64,
    pub selling_ends_ms: u64,
    pub spot: u128,
    pub spot_scale: u8,
}

#[event]
pub struct VaultPositionRedeemed {
    pub vault: Pubkey,
    pub round: u64,
    pub position: Pubkey,
    pub underlying: u64,
    pub settlement: u64,
}

#[event]
pub struct VaultRfqOpened {
    pub vault: Pubkey,
    pub round: u64,
    pub auction: Pubkey,
    pub slice_amount: u64,
    pub reserve_premium: u64,
}

#[event]
pub struct VaultRfqSettled {
    pub vault: Pubkey,
    pub round: u64,
    pub auction: Pubkey,
    pub position: Pubkey,
    pub amount: u64,
    pub net_premium: u64,
}

#[event]
pub struct VaultRfqUnsold {
    pub vault: Pubkey,
    pub round: u64,
    pub auction: Pubkey,
    pub amount: u64,
}

#[event]
pub struct VaultSwapOpened {
    pub vault: Pubkey,
    pub round: u64,
    pub auction: Pubkey,
    pub amount_s: u64,
    pub reserve_underlying: u64,
}

#[event]
pub struct VaultSwapSettled {
    pub vault: Pubkey,
    pub round: u64,
    pub auction: Pubkey,
    pub bidder: Pubkey,
    pub settlement_out: u64,
    pub underlying_in: u64,
}

#[event]
pub struct VaultSwapUnfilled {
    pub vault: Pubkey,
    pub round: u64,
    pub auction: Pubkey,
    pub amount_s: u64,
}

#[event]
pub struct VaultFeesCharged {
    pub vault: Pubkey,
    pub round: u64,
    pub mgmt_fee: u64,
    pub perf_fee: u64,
}

#[event]
pub struct VaultRoundFinalized {
    pub vault: Pubkey,
    pub round: u64,
    pub pps: u128,
    pub aum: u64,
    pub shares: u64,
    pub premium_s: u64,
    pub premium_u: u64,
    pub withdrawals_owed: u64,
    pub shares_burned: u64,
    pub deposits_processed: u64,
    pub shares_minted: u64,
}
