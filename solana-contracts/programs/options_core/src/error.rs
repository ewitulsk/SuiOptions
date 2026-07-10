use anchor_lang::prelude::*;

/// Core error codes. Ports `errors.move` 1:1 for the codes that belong to
/// the core program (venue/vault codes move to their own programs), plus
/// Solana-specific additions at the end.
#[error_code]
pub enum CoreError {
    #[msg("Quote valid_until_ms has passed")]
    QuoteExpired,
    #[msg("Nonce already consumed for this account")]
    QuoteNonceUsed,
    #[msg("Quote signature verification failed")]
    QuoteSignatureInvalid,
    #[msg("Quote protocol_id does not match config")]
    QuoteProtocolMismatch,
    #[msg("Quote bucket_id does not match provided bucket")]
    QuoteBucketMismatch,
    #[msg("Quote signer_account_id does not match provided account")]
    QuoteAccountMismatch,
    #[msg("Quote signer_token_recipient does not match flow recipient")]
    QuoteRecipientMismatch,
    #[msg("Operation requires now < expiry but bucket is expired")]
    BucketExpired,
    #[msg("Operation requires now >= expiry but bucket is not expired")]
    BucketNotExpired,
    #[msg("Bucket still holds balances or unredeemed positions")]
    BucketNotDrained,
    #[msg("Account lacks balance for quote")]
    InsufficientAccountBalance,
    #[msg("Provided amount does not match quote write_amount or premium")]
    AmountMismatch,
    #[msg("Exercise payment does not equal amount x strike")]
    SettlementAmountMismatch,
    #[msg("Exercise would advance cursor past total_written")]
    CursorOverflow,
    #[msg("Caller is not the owner")]
    NotOwner,
    #[msg("Position bucket does not match provided bucket")]
    PositionBucketMismatch,
    #[msg("Fee exceeds MAX_FEE_BPS")]
    FeeTooHigh,
    #[msg("Nonce is still valid; cannot prune")]
    NonceStillValid,
    #[msg("Treasury lacks balance")]
    InsufficientTreasuryBalance,
    #[msg("Amount must be positive")]
    ZeroAmount,
    #[msg("Unsupported signing scheme")]
    InvalidSigningScheme,
    #[msg("Signing pubkey has wrong length for scheme")]
    InvalidPubkeyLength,
    #[msg("strike_scale exceeds the supported maximum (38)")]
    StrikeScaleTooLarge,
    #[msg("Bucket is invalidated; new writes are frozen")]
    BucketInvalidated,
    #[msg("Bucket is not invalidated")]
    BucketNotInvalidated,
    #[msg("Put collateral does not equal ceil(write_amount x strike)")]
    PutCollateralMismatch,
    // ── Solana-specific ──
    #[msg("Arithmetic overflow in strike/fee math")]
    MathOverflow,
    #[msg("Expected Ed25519 signature-verification instruction not found or malformed")]
    MissingSigVerification,
    #[msg("Token account owner does not match expected recipient")]
    RecipientMismatch,
}
