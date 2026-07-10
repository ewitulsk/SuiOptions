use anchor_lang::prelude::*;

// Ports `events.move` for the core program. Sui `TypeName`s become mint
// pubkeys; Sui object `ID`s become account pubkeys. Emitted via
// `emit_cpi!` so the indexer reads them from inner-instruction data
// (log-truncation-proof).

#[event]
pub struct BucketCreated {
    pub bucket: Pubkey,
    pub underlying_mint: Pubkey,
    pub settlement_mint: Pubkey,
    pub call_mint: Pubkey,
    pub expiry_ms: u64,
    pub strike: u128,
    pub strike_scale: u8,
}

#[event]
pub struct WriteExecuted {
    pub bucket: Pubkey,
    pub signer_account: Pubkey,
    pub signer_token_recipient: Pubkey,
    pub executor: Pubkey,
    pub position: Pubkey,
    pub position_recipient: Pubkey,
    pub call_token_recipient: Pubkey,
    pub write_amount: u64,
    pub gross_premium: u64,
    pub fee: u64,
    pub net_premium: u64,
    pub range_start: u128,
    pub range_end: u128,
    pub nonce: u64,
}

#[event]
pub struct CollateralizedWrite {
    pub bucket: Pubkey,
    pub writer: Pubkey,
    pub position: Pubkey,
    pub amount: u64,
    pub range_start: u128,
    pub range_end: u128,
}

#[event]
pub struct Exercised {
    pub bucket: Pubkey,
    pub exerciser: Pubkey,
    pub amount: u64,
    pub settlement_paid: u64,
    pub cursor_after: u128,
}

#[event]
pub struct Redeemed {
    pub bucket: Pubkey,
    pub position: Pubkey,
    pub redeemer: Pubkey,
    pub range_start: u128,
    pub range_end: u128,
    pub underlying_returned: u64,
    pub settlement_returned: u64,
}

#[event]
pub struct ExpiredOptionBurned {
    pub bucket: Pubkey,
    pub burner: Pubkey,
    pub amount: u64,
}

#[event]
pub struct BucketCleaned {
    pub bucket: Pubkey,
}

#[event]
pub struct BucketInvalidated {
    pub bucket: Pubkey,
    pub timestamp_ms: u64,
    pub admin: Pubkey,
    pub reason: String,
}

#[event]
pub struct BucketRevalidated {
    pub bucket: Pubkey,
    pub timestamp_ms: u64,
    pub admin: Pubkey,
    pub reason: String,
}

// ── Puts ──

#[event]
pub struct PutBucketCreated {
    pub bucket: Pubkey,
    pub underlying_mint: Pubkey,
    pub settlement_mint: Pubkey,
    pub put_mint: Pubkey,
    pub expiry_ms: u64,
    pub strike: u128,
    pub strike_scale: u8,
}

#[event]
pub struct PutWriteExecuted {
    pub bucket: Pubkey,
    pub signer_account: Pubkey,
    pub signer_token_recipient: Pubkey,
    pub executor: Pubkey,
    pub position: Pubkey,
    pub position_recipient: Pubkey,
    pub put_token_recipient: Pubkey,
    pub write_amount: u64,
    pub collateral: u64,
    pub gross_premium: u64,
    pub fee: u64,
    pub net_premium: u64,
    pub range_start: u128,
    pub range_end: u128,
    pub nonce: u64,
}

#[event]
pub struct PutCollateralizedWrite {
    pub bucket: Pubkey,
    pub writer: Pubkey,
    pub position: Pubkey,
    pub write_amount: u64,
    pub collateral: u64,
    pub range_start: u128,
    pub range_end: u128,
}

#[event]
pub struct PutExercised {
    pub bucket: Pubkey,
    pub exerciser: Pubkey,
    pub amount: u64,
    pub settlement_paid: u64,
    pub cursor_after: u128,
}

#[event]
pub struct PutRedeemed {
    pub bucket: Pubkey,
    pub position: Pubkey,
    pub redeemer: Pubkey,
    pub range_start: u128,
    pub range_end: u128,
    pub underlying_returned: u64,
    pub settlement_returned: u64,
}

#[event]
pub struct PutExpiredOptionBurned {
    pub bucket: Pubkey,
    pub burner: Pubkey,
    pub amount: u64,
}

#[event]
pub struct PutBucketCleaned {
    pub bucket: Pubkey,
    pub dust_swept: u64,
}

#[event]
pub struct PutBucketInvalidated {
    pub bucket: Pubkey,
    pub timestamp_ms: u64,
    pub admin: Pubkey,
    pub reason: String,
}

#[event]
pub struct PutBucketRevalidated {
    pub bucket: Pubkey,
    pub timestamp_ms: u64,
    pub admin: Pubkey,
    pub reason: String,
}

// ── Accounts ──

#[event]
pub struct AccountCreated {
    pub account: Pubkey,
    pub owner: Pubkey,
    pub signing_scheme: u8,
    pub signing_pubkey: Vec<u8>,
}

#[event]
pub struct AccountDeposit {
    pub account: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
}

#[event]
pub struct AccountWithdraw {
    pub account: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
}

#[event]
pub struct SigningKeyRotated {
    pub account: Pubkey,
    pub new_scheme: u8,
    pub new_pubkey: Vec<u8>,
}

// ── Admin / treasury ──

#[event]
pub struct FeeUpdated {
    pub old_bps: u64,
    pub new_bps: u64,
}

#[event]
pub struct AdminChanged {
    pub old_admin: Pubkey,
    pub new_admin: Pubkey,
}

#[event]
pub struct TreasuryWithdrawn {
    pub mint: Pubkey,
    pub amount: u64,
    pub recipient: Pubkey,
}

#[event]
pub struct ProtocolFeeDeposited {
    pub mint: Pubkey,
    pub amount: u64,
    pub payer: Pubkey,
}

// ── Solana-specific ──

#[event]
pub struct PositionTransferred {
    pub position: Pubkey,
    pub old_owner: Pubkey,
    pub new_owner: Pubkey,
}
