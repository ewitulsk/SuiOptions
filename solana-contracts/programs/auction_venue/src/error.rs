use anchor_lang::prelude::*;

/// Venue error codes — ports the rfq/swap-auction rows of `errors.move`.
#[error_code]
pub enum VenueError {
    #[msg("Auction deadline has passed; no more bids")]
    AuctionClosed,
    #[msg("Auction deadline has not passed yet")]
    AuctionNotClosed,
    #[msg("Bid below reserve or minimum increment")]
    BidTooLow,
    #[msg("Auction duration below the minimum")]
    DurationTooShort,
    #[msg("Auction would end too close to bucket expiry")]
    TooCloseToExpiry,
    #[msg("Bucket is expired or invalidated")]
    BucketExpiredOrInvalid,
    #[msg("Bucket is still live; expired-recovery path unavailable")]
    BucketStillLive,
    #[msg("Missing or wrong settle authority for coupled auction")]
    WrongSettleAuthority,
    #[msg("Instruction does not match the auction mode")]
    WrongMode,
    #[msg("Amount must be positive")]
    ZeroAmount,
    #[msg("Account does not match the recipient fixed at creation")]
    RecipientMismatch,
    #[msg("Escrow does not equal the put's required collateral")]
    CollateralMismatch,
    #[msg("Arithmetic overflow")]
    MathOverflow,
    #[msg("Refund account is not the outbid bidder's ATA")]
    RefundAccountMismatch,
    #[msg("Account does not match the auction's bucket")]
    BucketMismatch,
    #[msg("force_refund requires the settle authority")]
    ForceRefundUnauthorized,
}
