//! Off-chain mirror of the on-chain error codes in §3.7, plus a handful of
//! validation errors the off-chain services raise locally (quote shape,
//! oversubscription) before a transaction is ever built.

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    // -- mirror of §3.7 --
    #[error("quote expired")]
    QuoteExpired,
    #[error("quote nonce already consumed")]
    QuoteNonceUsed,
    #[error("quote signature is invalid")]
    QuoteSignatureInvalid,
    #[error("quote protocol_id does not match config")]
    QuoteProtocolMismatch,
    #[error("quote bucket_id does not match provided bucket")]
    QuoteBucketMismatch,
    #[error("quote signer_account_id does not match provided account")]
    QuoteAccountMismatch,
    #[error("bucket has expired")]
    BucketExpired,
    #[error("bucket has not yet expired")]
    BucketNotExpired,
    #[error("bucket still holds balances")]
    BucketNotDrained,
    #[error("account has insufficient balance")]
    InsufficientAccountBalance,
    #[error("coin value does not match quote amount")]
    AmountMismatch,
    #[error("settlement payment does not equal amount × strike")]
    SettlementAmountMismatch,
    #[error("exercise would advance cursor past total_written")]
    CursorOverflow,
    #[error("caller is not the account owner")]
    NotOwner,
    #[error("position bucket_id does not match provided bucket")]
    PositionBucketMismatch,

    // -- local validation --
    #[error("quote already executed or reserved")]
    QuoteAlreadyReserved,
    #[error("unknown bucket")]
    UnknownBucket,
    #[error("unknown account")]
    UnknownAccount,
    #[error("bcs encoding error: {0}")]
    Bcs(String),

    #[error("unknown signing scheme tag (expected 0/1/2)")]
    InvalidSigningScheme,
}
