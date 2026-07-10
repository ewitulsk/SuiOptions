use anchor_lang::prelude::*;

pub const VAULT_SEED: &[u8] = b"vault";
pub const SHARE_MINT_SEED: &[u8] = b"share_mint";
pub const DEPLOYABLE_SEED: &[u8] = b"deployable";
pub const PENDING_SEED: &[u8] = b"pending";
pub const PROCEEDS_SEED: &[u8] = b"proceeds";
pub const WITHDRAWAL_SEED: &[u8] = b"withdrawal";
pub const CLAIMABLE_SEED: &[u8] = b"claimable";
pub const QUEUED_SEED: &[u8] = b"queued";
pub const ROUND_SEED: &[u8] = b"round";
pub const VAULT_POS_SEED: &[u8] = b"vault_pos";

/// Settle-crank headroom an auction needs before bucket expiry (mirrors
/// `vault::SETTLE_BUFFER_MS` / `rfq::SETTLE_BUFFER_MS`).
pub const SETTLE_BUFFER_MS: u64 = 600_000;

/// 365-day year for the mgmt-fee proration (mirrors vault-sim).
pub const YEAR_MS: u128 = 31_536_000_000;

// Config hard caps (doc 03 §9).
pub const MAX_MGMT_FEE_BPS: u64 = 500;
pub const MAX_PERF_FEE_BPS: u64 = 3_000;
pub const MAX_SWAP_SLIPPAGE_BPS: u64 = 500;

#[derive(AnchorSerialize, AnchorDeserialize, InitSpace, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Bucket expired (or genesis): redeeming positions, swapping
    /// proceeds, waiting for finalize.
    Settling,
    /// Round live: bucket may be selected, RFQs opened/settled until
    /// `selling_ends_ms`, then hold to expiry.
    Active,
}

/// Port of `vault::VaultConfig` — field-for-field.
#[derive(AnchorSerialize, AnchorDeserialize, InitSpace, Clone, Copy, Debug, PartialEq, Eq)]
pub struct VaultConfig {
    // Fees, charged only on profitable rounds.
    pub mgmt_fee_bps_annual: u64,
    pub perf_fee_bps: u64,
    // Round shape.
    pub round_ms: u64,
    pub selling_window_ms: u64,
    // Bucket-selection guardrails vs Pyth spot.
    pub min_strike_bps_over_spot: u64,
    pub max_strike_bps_over_spot: u64,
    pub min_expiry_lead_ms: u64,
    pub max_expiry_lead_ms: u64,
    // RFQ slice guardrails.
    pub min_reserve_premium_bps: u64,
    pub max_slice_amount: u64,
    pub max_open_rfqs: u64,
    pub rfq_duration_ms: u64,
    pub rfq_snipe_window_ms: u64,
    pub rfq_snipe_extension_ms: u64,
    pub rfq_max_extension_ms: u64,
    pub rfq_min_increment_bps: u64,
    // Proceeds / swap policy.
    pub hold_premium_in_settlement: bool,
    pub max_swap_slippage_bps: u64,
    // Oracle pinning.
    pub underlying_feed_id: [u8; 32],
    pub settlement_feed_id: [u8; 32],
    pub max_price_age_secs: u64,
    pub max_conf_bps: u64,
    pub underlying_decimals: u8,
    pub settlement_decimals: u8,
}

/// Port of `vault::Vault<U, S, VShare>`. Balances live in PDA-seeded token
/// accounts (an ATA can't hold three underlying sub-balances); the share
/// `TreasuryCap` becomes `share_mint` with this PDA as authority; the pps
/// `Table` becomes per-round `RoundState` PDAs; the positions
/// `ObjectTable` becomes `VaultPosition` index PDAs + head/tail.
#[account]
#[derive(InitSpace)]
pub struct Vault {
    /// The Move AdminCap holder, for config/pause (rotule via creator).
    pub admin: Pubkey,
    pub underlying_mint: Pubkey,
    pub settlement_mint: Pubkey,
    pub share_mint: Pubkey,
    pub config: VaultConfig,
    /// Stashed by `update_config`, applied at the next finalize so the
    /// admin cannot change rules mid-round.
    pub pending_config: Option<VaultConfig>,
    // ── round state machine ──
    pub round: u64,
    pub phase: Phase,
    pub current_bucket: Option<Pubkey>,
    /// Expiry of `current_bucket`; 0 when none.
    pub current_expiry_ms: u64,
    /// `open_rfq` forbidden after this.
    pub selling_ends_ms: u64,
    pub open_rfqs: u64,
    pub open_swap_rfqs: u64,
    // ── per-round working state ──
    pub positions_head: u64,
    pub positions_tail: u64,
    /// Net-of-protocol-fee premium collected this round, settlement units.
    pub round_premium_collected: u64,
    /// Cumulative swap legs this round, for the premium→underlying
    /// conversion at the round's realized rate.
    pub round_swap_settlement_out: u64,
    pub round_swap_underlying_in: u64,
    pub paused_deposits: bool,
    /// Monotonic per-vault auction salt for CPI-created auctions.
    pub auction_nonce: u64,
    pub salt: u64,
    pub bump: u8,
}

/// pps[r], set exactly once when round r finalizes (the Move `Table`
/// entry). `claim_shares` / `complete_withdraw` pass the round they need.
#[account]
#[derive(InitSpace)]
pub struct RoundState {
    pub vault: Pubkey,
    pub round: u64,
    pub pps: u128,
    pub bump: u8,
}

/// FIFO index entry for a Position held by the vault (the Move
/// `ObjectTable<u64, Position>` row). Created at settle_rfq, closed at
/// crank_redeem.
#[account]
#[derive(InitSpace)]
pub struct VaultPosition {
    pub vault: Pubkey,
    pub index: u64,
    pub position: Pubkey,
    pub bump: u8,
}

/// Claim ticket for a queued deposit: claimable at `pps[round − 1]` once
/// that price exists.
#[account]
#[derive(InitSpace)]
pub struct DepositReceipt {
    pub owner: Pubkey,
    pub vault: Pubkey,
    pub round: u64,
    pub amount: u64,
}

/// Two-step withdrawal ticket: pays `shares × pps[round] / PPS_SCALE`
/// once round `round` finalizes.
#[account]
#[derive(InitSpace)]
pub struct WithdrawReceipt {
    pub owner: Pubkey,
    pub vault: Pubkey,
    pub round: u64,
    pub shares: u64,
}

pub fn validate_config(config: &VaultConfig) -> bool {
    config.mgmt_fee_bps_annual <= MAX_MGMT_FEE_BPS
        && config.perf_fee_bps <= MAX_PERF_FEE_BPS
        && config.max_swap_slippage_bps <= MAX_SWAP_SLIPPAGE_BPS
        && config.min_strike_bps_over_spot < config.max_strike_bps_over_spot
        && config.min_expiry_lead_ms < config.max_expiry_lead_ms
        && config.round_ms > 0
        && config.selling_window_ms > 0
        && config.max_slice_amount > 0
        && config.max_open_rfqs > 0
        && config.max_price_age_secs > 0
        && config.max_conf_bps > 0
}
