use anchor_lang::prelude::*;

/// Vault error codes — ports the vault/oracle rows of `errors.move`.
#[error_code]
pub enum VaultError {
    #[msg("Operation not legal in the current phase")]
    WrongPhase,
    #[msg("Bucket is not the vault's selected bucket")]
    BucketNotSelected,
    #[msg("A bucket is already selected this round")]
    BucketAlreadySelected,
    #[msg("Selling window has closed")]
    SellingClosed,
    #[msg("Positions remain to be redeemed")]
    PositionsPending,
    #[msg("Open RFQs must settle before finalize")]
    RfqsOpen,
    #[msg("Round has not been finalized")]
    RoundNotFinalized,
    #[msg("Receipt does not belong to this vault/round")]
    ReceiptMismatch,
    #[msg("Bucket strike outside the configured band over spot")]
    StrikeOutOfBand,
    #[msg("Bucket expiry outside the configured lead window")]
    ExpiryOutOfBand,
    #[msg("Slice exceeds cap or deployable balance")]
    SliceTooLarge,
    #[msg("Too many open RFQs")]
    TooManyRfqs,
    #[msg("Deposits are paused")]
    DepositsPaused,
    #[msg("Auction does not originate from this vault")]
    WrongOrigin,
    #[msg("Oracle account is not the pinned feed")]
    OracleFeedMismatch,
    #[msg("Oracle price is stale")]
    OraclePriceStale,
    #[msg("Oracle confidence interval too wide")]
    OracleConfidence,
    #[msg("Oracle price invalid")]
    OraclePriceInvalid,
    #[msg("Settlement proceeds must be swapped before finalize")]
    ProceedsUnswapped,
    #[msg("Vault config out of bounds")]
    ConfigInvalid,
    #[msg("Bucket is invalidated")]
    BucketInvalidated,
    #[msg("Amount must be positive")]
    ZeroAmount,
    #[msg("Caller is not the vault admin")]
    NotAdmin,
    #[msg("Wrong FIFO index for crank")]
    WrongIndex,
    #[msg("Arithmetic overflow")]
    MathOverflow,
    #[msg("Account mismatch")]
    AccountMismatch,
}
