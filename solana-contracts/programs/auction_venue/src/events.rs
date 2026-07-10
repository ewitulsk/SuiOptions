use anchor_lang::prelude::*;

use crate::state::AuctionMode;

// Ports RfqCreated/RfqBid/RfqSettled/RfqExpiredUnsold and the SwapRfq*
// family onto the unified auction, discriminated by `mode`.

#[event]
pub struct AuctionCreated {
    pub auction: Pubkey,
    pub mode: AuctionMode,
    pub bucket: Pubkey,
    pub creator: Pubkey,
    pub escrow_mint: Pubkey,
    pub bid_mint: Pubkey,
    pub amount: u64,
    pub notional: u64,
    pub reserve_bid: u64,
    pub deadline_ms: u64,
    pub max_deadline_ms: u64,
    pub min_increment_bps: u64,
    pub settle_authority: Option<Pubkey>,
}

#[event]
pub struct AuctionBid {
    pub auction: Pubkey,
    pub bidder: Pubkey,
    pub token_recipient: Pubkey,
    pub bid: u64,
    pub previous_bid: u64,
    pub deadline_ms: u64,
}

#[event]
pub struct AuctionSettled {
    pub auction: Pubkey,
    pub mode: AuctionMode,
    pub bucket: Pubkey,
    pub winner: Pubkey,
    pub token_recipient: Pubkey,
    pub position: Pubkey,
    pub position_recipient: Pubkey,
    pub amount: u64,
    pub notional: u64,
    pub gross_bid: u64,
    pub fee: u64,
    pub net_proceeds: u64,
}

/// No winner, refund path, or expired-bucket recovery.
#[event]
pub struct AuctionUnsold {
    pub auction: Pubkey,
    pub mode: AuctionMode,
    pub bucket: Pubkey,
    pub amount: u64,
    pub reserve_bid: u64,
    /// True when a standing bid was refunded (out-of-band swap or dead
    /// bucket), false for a plain no-bid expiry.
    pub bid_refunded: bool,
}
