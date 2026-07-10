use anchor_lang::prelude::*;

pub const AUCTION_SEED: &[u8] = b"auction";
pub const ESCROW_SEED: &[u8] = b"escrow";
pub const BIDS_SEED: &[u8] = b"bids";

/// Minimum auction duration, so bidders can react to `AuctionCreated`
/// (mirrors `rfq::MIN_DURATION_MS`).
pub const MIN_DURATION_MS: u64 = 300_000;

/// Settle-crank headroom before bucket expiry (mirrors
/// `rfq::SETTLE_BUFFER_MS`).
pub const SETTLE_BUFFER_MS: u64 = 600_000;

#[derive(AnchorSerialize, AnchorDeserialize, InitSpace, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuctionMode {
    /// Pure token-for-token swap — zero dependency on the options
    /// protocol (subsumes `swap_auction.move`). Escrow → winner, winning
    /// bid → proceeds.
    Swap,
    /// Covered-call adapter (`rfq.move`): escrow is underlying; settle
    /// CPIs `options_core::write_collateralized` — winner gets call
    /// coins, `position_recipient` gets the Position, proceeds get the
    /// net premium.
    CoveredCall,
    /// Cash-secured-put adapter (`rfq_put.move`): escrow is the ceil
    /// cash collateral for `notional` underlying-units.
    CashSecuredPut,
}

/// One escrowed ascending auction — the unification of `rfq.move`,
/// `rfq_put.move` and `swap_auction.move` (same machinery, different
/// legs). Escrowed bids are what make the best bid always settleable,
/// which is what makes the settle crank permissionless.
#[account]
#[derive(InitSpace)]
pub struct Auction {
    pub creator: Pubkey,
    pub salt: u64,
    pub mode: AuctionMode,
    /// The options_core bucket for adapter modes; `Pubkey::default()` for
    /// pure swaps.
    pub bucket: Pubkey,
    pub escrow_mint: Pubkey,
    pub bid_mint: Pubkey,
    /// Escrowed amount (underlying for calls, cash collateral for puts,
    /// sell-side tokens for swaps).
    pub amount: u64,
    /// Option notional in underlying units (== amount for calls; the put
    /// notional for puts; 0 for swaps).
    pub notional: u64,
    /// Bids below this are rejected — the only price-safety floor a quiet
    /// auction has.
    pub reserve_bid: u64,
    pub deadline_ms: u64,
    /// Anti-snipe: a best bid inside `snipe_window_ms` of the deadline
    /// pushes it out by `snipe_extension_ms`, capped at `max_deadline_ms`.
    pub snipe_window_ms: u64,
    pub snipe_extension_ms: u64,
    pub max_deadline_ms: u64,
    /// Minimum improvement over the current best, in bps of the best.
    pub min_increment_bps: u64,
    /// The bid escrow token account's balance IS the best bid.
    pub best_bidder: Option<Pubkey>,
    /// Where the winner wants the option coins (call/put recipient wallet)
    /// or the swapped escrow (swap mode).
    pub best_token_recipient: Option<Pubkey>,
    /// Owner of the minted Position (adapter modes).
    pub position_recipient: Pubkey,
    /// Exact token account receiving the winning bid (net premium for
    /// adapters, the bid tokens for swaps). Fixed at creation so a
    /// coupled venue (the vault) can absorb into its own vaults.
    pub proceeds_token: Pubkey,
    /// Exact token account receiving the escrow back on the no-winner /
    /// refund / expired paths.
    pub refund_token: Pubkey,
    /// When set, only this authority (a coupled venue's PDA, signing via
    /// CPI) may settle — the analog of `rfq.move`'s `coupled` flag.
    pub settle_authority: Option<Pubkey>,
    pub bump: u8,
}
