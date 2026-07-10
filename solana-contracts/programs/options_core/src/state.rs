use anchor_lang::prelude::*;

pub const CONFIG_SEED: &[u8] = b"config";
pub const TREASURY_SEED: &[u8] = b"treasury";
pub const MM_ACCOUNT_SEED: &[u8] = b"mm_account";
pub const NONCE_SEED: &[u8] = b"nonce";
pub const BUCKET_SEED: &[u8] = b"bucket";
pub const PUT_BUCKET_SEED: &[u8] = b"put_bucket";
pub const CALL_MINT_SEED: &[u8] = b"call_mint";
pub const PUT_MINT_SEED: &[u8] = b"put_mint";

/// Hard cap on the protocol fee (mirrors `admin::MAX_FEE_BPS`).
pub const MAX_FEE_BPS: u64 = 1000;

/// Signing schemes for MM quote keys. Only Ed25519 is implemented in v1;
/// the field is kept so adding secp256k1/r1 later is append-only (see the
/// port plan's open decision #1).
pub const SCHEME_ED25519: u8 = 0;

pub const ED25519_PUBKEY_LEN: usize = 32;
/// Room for a compressed secp pubkey if a scheme is added later.
pub const MAX_PUBKEY_LEN: usize = 33;

/// Protocol config — the Sui `ProtocolConfig` + `AdminCap` fused: the
/// capability becomes an `admin` pubkey (rotatable via `set_admin`, the
/// analog of transferring the cap). The config PDA's own address is the
/// quote domain separator (Sui derived `protocol_id` from the AdminCap id).
#[account]
#[derive(InitSpace)]
pub struct Config {
    pub admin: Pubkey,
    pub fee_bps: u64,
    pub bump: u8,
}

/// Fee treasury marker. Balances live in associated token accounts owned
/// by this PDA — the analog of Sui's dynamic-field `Balance<T>` bag.
#[account]
#[derive(InitSpace)]
pub struct Treasury {
    pub bump: u8,
}

/// Market-maker account (Sui shared `Account`). Balances live in ATAs
/// owned by this PDA; consumed nonces live in `NonceRecord` PDAs.
#[account]
#[derive(InitSpace)]
pub struct MmAccount {
    pub owner: Pubkey,
    /// Distinguishes multiple accounts per owner (the future MM-sharding
    /// pattern from the spec).
    pub salt: u64,
    pub signing_scheme: u8,
    #[max_len(MAX_PUBKEY_LEN)]
    pub signing_pubkey: Vec<u8>,
    pub bump: u8,
}

/// A consumed quote nonce (Sui dynamic field `NonceKey → valid_until_ms`).
/// `init` failing on an existing PDA IS the replay check. `prune_nonce`
/// closes it after expiry, rent to the caller — the incentive analog of
/// Sui's storage rebate.
#[account]
#[derive(InitSpace)]
pub struct NonceRecord {
    pub mm_account: Pubkey,
    pub nonce: u64,
    pub valid_until_ms: u64,
    pub bump: u8,
}

/// Covered-call bucket (Sui `Bucket<U, S, C>`). The generics become stored
/// mint pubkeys; the `TreasuryCap<Call>` becomes `call_mint` with this PDA
/// as sole mint/burn authority, so outstanding supply == outstanding
/// options exactly as on Sui. Bucket isolation is the runtime constraint
/// `token_account.mint == bucket.call_mint` (checked on every mint/burn
/// path) instead of a type-system guarantee — an explicit audit item.
#[account]
#[derive(InitSpace)]
pub struct Bucket {
    pub underlying_mint: Pubkey,
    pub settlement_mint: Pubkey,
    pub call_mint: Pubkey,
    pub expiry_ms: u64,
    /// Real ratio (settlement smallest-units per underlying smallest-unit)
    /// = `strike / 10^strike_scale`.
    pub strike: u128,
    pub strike_scale: u8,
    pub total_written: u128,
    pub exercise_cursor: u128,
    /// Admin-controlled freeze on new writes; exercises and redeems are
    /// unaffected.
    pub invalidated: bool,
    /// PDA-seed salt so an identical (pair, expiry, strike) bucket can be
    /// re-created if one is invalidated (Sui allowed duplicates freely).
    pub salt: u64,
    pub bump: u8,
}

/// Cash-secured-put bucket (Sui `PutBucket<U, S, P>`). Same cursor design
/// with the asset legs flipped: collateral is settlement cash, exercisers
/// deliver underlying. `total_written` / cursor / ranges are denominated in
/// UNDERLYING units, exactly as for calls.
#[account]
#[derive(InitSpace)]
pub struct PutBucket {
    pub underlying_mint: Pubkey,
    pub settlement_mint: Pubkey,
    pub put_mint: Pubkey,
    pub expiry_ms: u64,
    pub strike: u128,
    pub strike_scale: u8,
    pub total_written: u128,
    pub exercise_cursor: u128,
    /// Sum of redeemed position ranges; cleanup requires it to equal
    /// `total_written` (the cash leg leaves rounding dust, so a
    /// zero-balance gate would be unreachable for fractional strikes).
    pub total_redeemed: u128,
    pub invalidated: bool,
    pub salt: u64,
    pub bump: u8,
}

/// Writer position over `[range_start, range_end)` (Sui owned `Position`
/// object). A plain owned record with a `transfer_position` instruction —
/// not an NFT (port plan decision #2). Created with a fresh keypair, like
/// Sui object ids; closed at redeem with rent to the redeemer. Both call
/// and put buckets mint this type; `bucket` disambiguates.
#[account]
#[derive(InitSpace)]
pub struct Position {
    pub owner: Pubkey,
    pub bucket: Pubkey,
    pub range_start: u128,
    pub range_end: u128,
}
