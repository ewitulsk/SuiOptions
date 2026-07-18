//! Off-chain mirror of the Move events in §3.5.
//!
//! These are produced by the indexer (parsed out of Sui's event stream;
//! synthesized by the stub source until the contracts are deployed) and
//! consumed by anything downstream: the quoting service's state, frontends,
//! historical readers.
//!
//! The structs are JSON-shaped — they're not BCS-signed, just transported
//! over WS/JSON. Numeric fields use the decimal-string adapters to dodge JS
//! precision loss.

use serde::{Deserialize, Serialize};

use super::asset::AssetType;
use super::coding::{u128_string, u64_string};
use super::ids::{ObjectId, SuiAddress};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketCreated {
    pub bucket_id: ObjectId,
    pub asset_type: AssetType,
    pub settlement_type: AssetType,
    /// Fully-qualified type of the per-bucket fungible option coin
    /// (`Coin<call_type>`). BCS-matches the on-chain `TypeName` field.
    pub call_type: AssetType,
    #[serde(with = "u64_string")]
    pub expiry_ms: u64,
    /// Scaled strike. Real ratio = `strike / 10^strike_scale`
    /// (settlement-smallest-units per underlying-smallest-unit). u128 to
    /// match the on-chain Bucket field.
    #[serde(with = "u128_string")]
    pub strike: u128,
    /// 0..=9. Caps at 9 to match Pyth's normalized convention.
    pub strike_scale: u8,
}

impl BucketCreated {
    /// Real ratio as an f64. Lossy for very large strikes; convenient
    /// for log lines and UI. On-chain math always uses the raw integers.
    pub fn strike_as_f64(&self) -> f64 {
        self.strike as f64 / 10f64.powi(self.strike_scale as i32)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteExecuted {
    pub bucket_id: ObjectId,
    /// The `QuoteSigner` whose quote authorized this write.
    pub signer_id: ObjectId,
    /// The external collateral object the signer's funds released from.
    pub collateral_source: ObjectId,
    pub signer_token_recipient: SuiAddress,
    pub executor: SuiAddress,
    pub position_id: ObjectId,
    pub position_recipient: SuiAddress,
    pub call_token_recipient: SuiAddress,
    #[serde(with = "u64_string")]
    pub write_amount: u64,
    #[serde(with = "u64_string")]
    pub gross_premium: u64,
    #[serde(with = "u64_string")]
    pub fee: u64,
    #[serde(with = "u64_string")]
    pub net_premium: u64,
    #[serde(with = "u128_string")]
    pub range_start: u128,
    #[serde(with = "u128_string")]
    pub range_end: u128,
    #[serde(with = "u64_string")]
    pub nonce: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exercised {
    pub bucket_id: ObjectId,
    pub exerciser: SuiAddress,
    #[serde(with = "u64_string")]
    pub amount: u64,
    #[serde(with = "u64_string")]
    pub settlement_paid: u64,
    #[serde(with = "u128_string")]
    pub cursor_after: u128,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Redeemed {
    pub bucket_id: ObjectId,
    pub position_id: ObjectId,
    pub redeemer: SuiAddress,
    #[serde(with = "u128_string")]
    pub range_start: u128,
    #[serde(with = "u128_string")]
    pub range_end: u128,
    #[serde(with = "u64_string")]
    pub underlying_returned: u64,
    #[serde(with = "u64_string")]
    pub settlement_returned: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpiredOptionBurned {
    pub bucket_id: ObjectId,
    pub burner: SuiAddress,
    #[serde(with = "u64_string")]
    pub amount: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketCleaned {
    pub bucket_id: ObjectId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketInvalidated {
    pub bucket_id: ObjectId,
    #[serde(with = "u64_string")]
    pub at_ms: u64,
    pub admin: SuiAddress,
    #[serde(with = "crate::coding::bytes_hex")]
    pub reason: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketRevalidated {
    pub bucket_id: ObjectId,
    #[serde(with = "u64_string")]
    pub at_ms: u64,
    pub admin: SuiAddress,
    #[serde(with = "crate::coding::bytes_hex")]
    pub reason: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignerCreated {
    pub signer_id: ObjectId,
    pub owner: SuiAddress,
    /// Tag for the registered signing key. BCS-encodes as a single u8;
    /// must match the on-chain struct field order in `events.move`.
    pub signing_scheme: crate::SigningScheme,
    #[serde(with = "crate::coding::bytes_hex")]
    pub signing_pubkey: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigningKeyRotated {
    pub signer_id: ObjectId,
    pub new_scheme: crate::SigningScheme,
    #[serde(with = "crate::coding::bytes_hex")]
    pub new_pubkey: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeUpdated {
    #[serde(with = "u64_string")]
    pub old_bps: u64,
    #[serde(with = "u64_string")]
    pub new_bps: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreasuryWithdrawn {
    pub asset_type: AssetType,
    #[serde(with = "u64_string")]
    pub amount: u64,
    pub recipient: SuiAddress,
}

/// A DeepBook v3 pool created for one of OUR buckets' call coins (SO-152).
///
/// Unlike the other events this is NOT a BCS mirror of a single Move struct:
/// DeepBook's `pool::PoolCreated<Base, Quote>` carries the asset types only
/// in the event *type string* generics, and `bucket_id` is resolved by the
/// indexer (bucket whose `call_type` == the pool's base asset). The indexer
/// decodes the raw payload, parses the generics, and emits this enriched
/// form; pools whose base asset is not a known call coin are dropped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeepBookPoolCreated {
    pub pool_id: ObjectId,
    /// Resolved off-chain: the bucket whose `call_type` is `base_asset_type`.
    pub bucket_id: ObjectId,
    pub base_asset_type: AssetType,
    pub quote_asset_type: AssetType,
    #[serde(with = "u64_string")]
    pub tick_size: u64,
    #[serde(with = "u64_string")]
    pub lot_size: u64,
    #[serde(with = "u64_string")]
    pub min_size: u64,
    #[serde(with = "u64_string")]
    pub taker_fee: u64,
    #[serde(with = "u64_string")]
    pub maker_fee: u64,
}

/// A DeepBook v3 `order_info::OrderFilled` for one of OUR buckets' call coins,
/// enriched off-chain (SO-209). One on-chain fill emits one of these per maker
/// order crossed. Like [`DeepBookPoolCreated`] this is NOT a raw BCS mirror:
/// `bucket_id` is resolved by the indexer (pool → bucket), and only fills on a
/// known bucket's pool are emitted. The taker/maker `BalanceManager` ids are
/// the on-chain trading-account handles — the api-service maps them back to a
/// wallet (the frontend passes its own BM id) to attribute cost basis.
///
/// Side semantics: `taker_is_bid` true ⇒ the taker BOUGHT `base_quantity` for
/// `quote_quantity` (maker sold); false ⇒ the maker bought (taker sold). Fees
/// are charged amounts (not rates), in DEEP when the matching `*_fee_is_deep`
/// is true, else in the input token (settlement coin) — so a settlement-token
/// fee adds to a buyer's cost / subtracts from a seller's proceeds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeepBookOrderFilled {
    pub pool_id: ObjectId,
    /// Resolved off-chain: the bucket whose `call_type` is the pool's base.
    pub bucket_id: ObjectId,
    pub taker_balance_manager_id: ObjectId,
    pub maker_balance_manager_id: ObjectId,
    pub taker_is_bid: bool,
    #[serde(with = "u64_string")]
    pub base_quantity: u64,
    #[serde(with = "u64_string")]
    pub quote_quantity: u64,
    #[serde(with = "u64_string")]
    pub price: u64,
    #[serde(with = "u64_string")]
    pub taker_fee: u64,
    pub taker_fee_is_deep: bool,
    #[serde(with = "u64_string")]
    pub maker_fee: u64,
    pub maker_fee_is_deep: bool,
    #[serde(with = "u64_string")]
    pub timestamp_ms: u64,
}

// ─── write-core / RFQ events (vault-implementation-guide docs 01–02) ───

/// Self-write / venue escrow write (`bucket::write_collateralized`).
/// Deliberately distinct from `WriteExecuted`: no premium, no signer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollateralizedWrite {
    pub bucket_id: ObjectId,
    pub writer: SuiAddress,
    #[serde(with = "u64_string")]
    pub amount: u64,
    #[serde(with = "u128_string")]
    pub range_start: u128,
    #[serde(with = "u128_string")]
    pub range_end: u128,
}

// ─── generic auction venue (auction package, `{auction_pkg}::events`) ───

/// One event set for every auction regardless of asset pair or coupling;
/// `escrow_type` / `bid_type` carry the legs for indexing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuctionCreated {
    pub auction_id: ObjectId,
    /// Vault ID (coupled auctions) or caller-supplied attribution.
    pub origin: ObjectId,
    /// Escrowed leg's coin type. BCS-matches the on-chain `TypeName` field.
    pub escrow_type: AssetType,
    /// Bid leg's coin type.
    pub bid_type: AssetType,
    #[serde(with = "u64_string")]
    pub amount: u64,
    #[serde(with = "u64_string")]
    pub reserve_bid: u64,
    #[serde(with = "u64_string")]
    pub deadline_ms: u64,
    #[serde(with = "u64_string")]
    pub max_deadline_ms: u64,
    #[serde(with = "u64_string")]
    pub min_increment_bps: u64,
    /// Coupled auctions are consumed by their venue (vault); uncoupled ones
    /// settle through the generic path (`AuctionSettled`).
    pub coupled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuctionBid {
    pub auction_id: ObjectId,
    pub bidder: SuiAddress,
    pub token_recipient: SuiAddress,
    #[serde(with = "u64_string")]
    pub amount: u64,
    /// 0 if this was the first bid.
    #[serde(with = "u64_string")]
    pub previous_best: u64,
    /// Post-anti-snipe deadline.
    #[serde(with = "u64_string")]
    pub deadline_ms: u64,
}

/// Emitted by the uncoupled `settle` path only. Coupled venues emit their
/// own settlement events (`VaultRfqSettled`, `SwapRfqSettled`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuctionSettled {
    pub auction_id: ObjectId,
    pub origin: ObjectId,
    pub bidder: SuiAddress,
    pub token_recipient: SuiAddress,
    #[serde(with = "u64_string")]
    pub amount: u64,
    #[serde(with = "u64_string")]
    pub winning_bid: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuctionUnfilled {
    pub auction_id: ObjectId,
    pub origin: ObjectId,
    #[serde(with = "u64_string")]
    pub amount: u64,
    #[serde(with = "u64_string")]
    pub reserve_bid: u64,
}

// ─── option-RFQ adapter (options_rfq package, `{rfq_pkg}::events`) ───

/// Adapter-level creation event: links the option-RFQ metadata object to
/// its generic auction (which emits its own `AuctionCreated` with the
/// deadline/increment params) and the bucket it will write into.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RfqCreated {
    pub rfq_id: ObjectId,
    pub auction_id: ObjectId,
    pub bucket_id: ObjectId,
    /// Caller-supplied attribution (seller address-as-ID).
    pub origin: ObjectId,
    #[serde(with = "u64_string")]
    pub amount: u64,
    #[serde(with = "u64_string")]
    pub reserve_premium: u64,
}

/// Mirrors `WriteExecuted`'s economic fields so the positions
/// materializer can treat both as "a position was minted with premium X".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RfqSettled {
    pub rfq_id: ObjectId,
    pub auction_id: ObjectId,
    pub bucket_id: ObjectId,
    pub origin: ObjectId,
    pub winner: SuiAddress,
    pub call_recipient: SuiAddress,
    pub position_id: ObjectId,
    pub position_recipient: SuiAddress,
    #[serde(with = "u64_string")]
    pub amount: u64,
    #[serde(with = "u64_string")]
    pub gross_premium: u64,
    #[serde(with = "u64_string")]
    pub fee: u64,
    #[serde(with = "u64_string")]
    pub net_premium: u64,
    #[serde(with = "u128_string")]
    pub range_start: u128,
    #[serde(with = "u128_string")]
    pub range_end: u128,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RfqExpiredUnsold {
    pub rfq_id: ObjectId,
    pub auction_id: ObjectId,
    pub bucket_id: ObjectId,
    pub origin: ObjectId,
    #[serde(with = "u64_string")]
    pub amount: u64,
    #[serde(with = "u64_string")]
    pub reserve_premium: u64,
}

// ─── vault-coupled RFQ settles (options_vault package) ───

/// A vault-coupled RFQ auction settled into a covered write. Mirrors the
/// adapter's `RfqSettled` economics; the auction's creation and bids are
/// the generic `Auction*` events. The minted `Position` stays with the
/// vault (no `position_recipient` field).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultRfqSettled {
    pub auction_id: ObjectId,
    pub bucket_id: ObjectId,
    pub vault_id: ObjectId,
    #[serde(with = "u64_string")]
    pub round: u64,
    pub winner: SuiAddress,
    pub call_recipient: SuiAddress,
    pub position_id: ObjectId,
    #[serde(with = "u64_string")]
    pub amount: u64,
    #[serde(with = "u64_string")]
    pub gross_premium: u64,
    #[serde(with = "u64_string")]
    pub fee: u64,
    #[serde(with = "u64_string")]
    pub net_premium: u64,
    #[serde(with = "u128_string")]
    pub range_start: u128,
    #[serde(with = "u128_string")]
    pub range_end: u128,
}

/// A vault-coupled RFQ auction resolved without a write: no bids, or the
/// bucket expired/was invalidated mid-auction (escrows recovered).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultRfqUnsold {
    pub auction_id: ObjectId,
    pub bucket_id: ObjectId,
    pub vault_id: ObjectId,
    #[serde(with = "u64_string")]
    pub round: u64,
    #[serde(with = "u64_string")]
    pub amount: u64,
    #[serde(with = "u64_string")]
    pub reserve_premium: u64,
}

// ─── proceeds-swap settles (options_vault package) ───
//
// Swap creation/bids surface as generic `AuctionCreated`/`AuctionBid`
// (origin = vault id, escrow_type = settlement coin, bid_type = underlying).

/// A swap auction filled in-band: `vault_id == origin`; carries `round`
/// for the round-economics materializer (realized swap rate → perf fee).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapRfqSettled {
    pub swap_id: ObjectId,
    pub vault_id: ObjectId,
    #[serde(with = "u64_string")]
    pub round: u64,
    pub winner: SuiAddress,
    #[serde(with = "u64_string")]
    pub settlement_filled: u64,
    #[serde(with = "u64_string")]
    pub underlying_in: u64,
}

/// A swap auction closed without converting (no bid, or the best bid fell
/// out of the Pyth band before settle). Settlement returns to proceeds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapRfqUnfilled {
    pub swap_id: ObjectId,
    pub vault_id: ObjectId,
    #[serde(with = "u64_string")]
    pub round: u64,
    #[serde(with = "u64_string")]
    pub amount_s: u64,
}

// ─── vault events (vault-implementation-guide doc 03) ───

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultCreated {
    pub vault_id: ObjectId,
    pub underlying_type: AssetType,
    pub settlement_type: AssetType,
    pub share_type: AssetType,
    // Genesis config snapshot (consumer-facing subset). See `VaultConfigApplied`.
    #[serde(with = "u64_string")]
    pub mgmt_fee_bps_annual: u64,
    #[serde(with = "u64_string")]
    pub perf_fee_bps: u64,
    #[serde(with = "u64_string")]
    pub round_ms: u64,
    #[serde(with = "u64_string")]
    pub selling_window_ms: u64,
    #[serde(with = "u64_string")]
    pub min_strike_bps_over_spot: u64,
    #[serde(with = "u64_string")]
    pub max_strike_bps_over_spot: u64,
}

/// Active config snapshot at a finalize boundary (`VaultConfigApplied`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultConfigApplied {
    pub vault_id: ObjectId,
    #[serde(with = "u64_string")]
    pub round: u64,
    #[serde(with = "u64_string")]
    pub mgmt_fee_bps_annual: u64,
    #[serde(with = "u64_string")]
    pub perf_fee_bps: u64,
    #[serde(with = "u64_string")]
    pub round_ms: u64,
    #[serde(with = "u64_string")]
    pub selling_window_ms: u64,
    #[serde(with = "u64_string")]
    pub min_strike_bps_over_spot: u64,
    #[serde(with = "u64_string")]
    pub max_strike_bps_over_spot: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultDeposit {
    pub vault_id: ObjectId,
    pub depositor: SuiAddress,
    /// The round the deposit participates from (receipt round).
    #[serde(with = "u64_string")]
    pub round: u64,
    #[serde(with = "u64_string")]
    pub amount: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharesClaimed {
    pub vault_id: ObjectId,
    pub owner: SuiAddress,
    #[serde(with = "u64_string")]
    pub round: u64,
    #[serde(with = "u64_string")]
    pub amount: u64,
    #[serde(with = "u64_string")]
    pub shares: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WithdrawInitiated {
    pub vault_id: ObjectId,
    pub owner: SuiAddress,
    #[serde(with = "u64_string")]
    pub round: u64,
    #[serde(with = "u64_string")]
    pub shares: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WithdrawCompleted {
    pub vault_id: ObjectId,
    pub owner: SuiAddress,
    #[serde(with = "u64_string")]
    pub round: u64,
    #[serde(with = "u64_string")]
    pub shares: u64,
    #[serde(with = "u64_string")]
    pub amount: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstantWithdraw {
    pub vault_id: ObjectId,
    pub owner: SuiAddress,
    #[serde(with = "u64_string")]
    pub round: u64,
    #[serde(with = "u64_string")]
    pub amount: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultBucketSelected {
    pub vault_id: ObjectId,
    #[serde(with = "u64_string")]
    pub round: u64,
    pub bucket_id: ObjectId,
    #[serde(with = "u128_string")]
    pub strike: u128,
    pub strike_scale: u8,
    #[serde(with = "u64_string")]
    pub expiry_ms: u64,
    #[serde(with = "u64_string")]
    pub selling_ends_ms: u64,
    /// Pyth cross at selection, at `spot_scale`.
    #[serde(with = "u128_string")]
    pub spot: u128,
    pub spot_scale: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultPositionRedeemed {
    pub vault_id: ObjectId,
    #[serde(with = "u64_string")]
    pub round: u64,
    pub position_id: ObjectId,
    #[serde(with = "u64_string")]
    pub underlying_returned: u64,
    #[serde(with = "u64_string")]
    pub settlement_returned: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultFeesCharged {
    pub vault_id: ObjectId,
    #[serde(with = "u64_string")]
    pub round: u64,
    #[serde(with = "u64_string")]
    pub mgmt_fee: u64,
    #[serde(with = "u64_string")]
    pub perf_fee: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultRoundFinalized {
    pub vault_id: ObjectId,
    /// The round that was finalized (the pps index).
    #[serde(with = "u64_string")]
    pub round: u64,
    #[serde(with = "u128_string")]
    pub pps: u128,
    #[serde(with = "u64_string")]
    pub aum: u64,
    #[serde(with = "u64_string")]
    pub shares: u64,
    #[serde(with = "u64_string")]
    pub premium_collected: u64,
    #[serde(with = "u64_string")]
    pub premium_underlying: u64,
    #[serde(with = "u64_string")]
    pub withdrawals_owed: u64,
    #[serde(with = "u64_string")]
    pub shares_burned: u64,
    #[serde(with = "u64_string")]
    pub deposits_processed: u64,
    #[serde(with = "u64_string")]
    pub shares_minted: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultConfigUpdated {
    pub vault_id: ObjectId,
    /// Configs apply at the next finalize; this is the current round.
    #[serde(with = "u64_string")]
    pub round: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultDepositsPaused {
    pub vault_id: ObjectId,
    pub paused: bool,
}

// ─── cash-secured put events (mirror of the call/RFQ events above) ───
//
// Field order matches the Move structs in `events.move` exactly. Puts carry
// two extra economic fields vs calls: `collateral` (the cash escrowed =
// ceil(amount × strike)) on the write events, and `dust_swept` on cleanup.

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutBucketCreated {
    pub bucket_id: ObjectId,
    pub asset_type: AssetType,
    pub settlement_type: AssetType,
    /// Fully-qualified type of the per-bucket fungible put coin (`Coin<put_type>`).
    pub put_type: AssetType,
    #[serde(with = "u64_string")]
    pub expiry_ms: u64,
    #[serde(with = "u128_string")]
    pub strike: u128,
    pub strike_scale: u8,
}

impl PutBucketCreated {
    pub fn strike_as_f64(&self) -> f64 {
        self.strike as f64 / 10f64.powi(self.strike_scale as i32)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutWriteExecuted {
    pub bucket_id: ObjectId,
    /// The `QuoteSigner` whose quote authorized this write.
    pub signer_id: ObjectId,
    /// The external collateral object the signer's funds released from.
    pub collateral_source: ObjectId,
    pub signer_token_recipient: SuiAddress,
    pub executor: SuiAddress,
    pub position_id: ObjectId,
    pub position_recipient: SuiAddress,
    pub put_token_recipient: SuiAddress,
    #[serde(with = "u64_string")]
    pub write_amount: u64,
    /// Cash collateral escrowed = ceil(write_amount × strike).
    #[serde(with = "u64_string")]
    pub collateral: u64,
    #[serde(with = "u64_string")]
    pub gross_premium: u64,
    #[serde(with = "u64_string")]
    pub fee: u64,
    #[serde(with = "u64_string")]
    pub net_premium: u64,
    #[serde(with = "u128_string")]
    pub range_start: u128,
    #[serde(with = "u128_string")]
    pub range_end: u128,
    #[serde(with = "u64_string")]
    pub nonce: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutCollateralizedWrite {
    pub bucket_id: ObjectId,
    pub writer: SuiAddress,
    #[serde(with = "u64_string")]
    pub write_amount: u64,
    #[serde(with = "u64_string")]
    pub collateral: u64,
    #[serde(with = "u128_string")]
    pub range_start: u128,
    #[serde(with = "u128_string")]
    pub range_end: u128,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutExercised {
    pub bucket_id: ObjectId,
    pub exerciser: SuiAddress,
    /// Underlying delivered in (== put coins burned).
    #[serde(with = "u64_string")]
    pub amount: u64,
    /// Settlement (cash) paid out = floor(amount × strike).
    #[serde(with = "u64_string")]
    pub settlement_paid: u64,
    #[serde(with = "u128_string")]
    pub cursor_after: u128,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutRedeemed {
    pub bucket_id: ObjectId,
    pub position_id: ObjectId,
    pub redeemer: SuiAddress,
    #[serde(with = "u128_string")]
    pub range_start: u128,
    #[serde(with = "u128_string")]
    pub range_end: u128,
    /// Assigned (exercised) underlying handed to the writer.
    #[serde(with = "u64_string")]
    pub underlying_returned: u64,
    /// Unassigned cash collateral returned = floor(unexercised × strike).
    #[serde(with = "u64_string")]
    pub settlement_returned: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutExpiredOptionBurned {
    pub bucket_id: ObjectId,
    pub burner: SuiAddress,
    #[serde(with = "u64_string")]
    pub amount: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutBucketCleaned {
    pub bucket_id: ObjectId,
    /// Rounding-remainder cash swept to the admin at cleanup.
    #[serde(with = "u64_string")]
    pub dust_swept: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutBucketInvalidated {
    pub bucket_id: ObjectId,
    #[serde(with = "u64_string")]
    pub at_ms: u64,
    pub admin: SuiAddress,
    #[serde(with = "crate::coding::bytes_hex")]
    pub reason: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutBucketRevalidated {
    pub bucket_id: ObjectId,
    #[serde(with = "u64_string")]
    pub at_ms: u64,
    pub admin: SuiAddress,
    #[serde(with = "crate::coding::bytes_hex")]
    pub reason: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutRfqCreated {
    pub rfq_id: ObjectId,
    pub auction_id: ObjectId,
    pub bucket_id: ObjectId,
    pub origin: ObjectId,
    /// Option notional in underlying units.
    #[serde(with = "u64_string")]
    pub amount: u64,
    /// Cash collateral escrowed = ceil(amount × strike).
    #[serde(with = "u64_string")]
    pub collateral: u64,
    #[serde(with = "u64_string")]
    pub reserve_premium: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutRfqSettled {
    pub rfq_id: ObjectId,
    pub auction_id: ObjectId,
    pub bucket_id: ObjectId,
    pub origin: ObjectId,
    pub winner: SuiAddress,
    pub put_recipient: SuiAddress,
    pub position_id: ObjectId,
    pub position_recipient: SuiAddress,
    #[serde(with = "u64_string")]
    pub amount: u64,
    #[serde(with = "u64_string")]
    pub gross_premium: u64,
    #[serde(with = "u64_string")]
    pub fee: u64,
    #[serde(with = "u64_string")]
    pub net_premium: u64,
    #[serde(with = "u128_string")]
    pub range_start: u128,
    #[serde(with = "u128_string")]
    pub range_end: u128,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutRfqExpiredUnsold {
    pub rfq_id: ObjectId,
    pub auction_id: ObjectId,
    pub bucket_id: ObjectId,
    pub origin: ObjectId,
    #[serde(with = "u64_string")]
    pub amount: u64,
    #[serde(with = "u64_string")]
    pub reserve_premium: u64,
}

/// Tagged union over every event the indexer may publish.
///
/// The variant name is what shows up as `"type"` over the wire; the payload
/// rides under `"payload"`. This is the same envelope shape as the WS
/// retail/MM messages so a generic event reader can treat both alike.

// ═══════════════════ curated trading vaults (SO-282) ═══════════════════
//
// Emitted by the `trading_vault` package (modules `events` + `vault_mm`)
// and its adapter packages (`deepbook_adapter`, `options_adapter`). Rust
// names carry a `Tv` prefix because several Move names (VaultCreated,
// DepositsPaused, RfqSettled) collide with options_vault events — the
// dispatch table disambiguates by package id, the enum needs distinct
// variants.

/// One `VecMap<TypeName, u64>` entry (BCS: struct Entry { key, value }).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvTypeAmount {
    pub key: AssetType,
    #[serde(with = "u64_string")]
    pub value: u64,
}

/// BCS shape of Move's `VecMap<TypeName, u64>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvTypeAmountMap {
    pub contents: Vec<TvTypeAmount>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvVaultCreated {
    pub vault_id: ObjectId,
    pub creator: SuiAddress,
    pub curator: SuiAddress,
    pub curator_cap_id: ObjectId,
    pub deposit_asset: AssetType,
    #[serde(with = "u64_string")]
    pub lockup_ms: u64,
    #[serde(with = "u64_string")]
    pub curator_fee_bps: u64,
    pub rotation_authority: u8,
    #[serde(with = "u64_string")]
    pub max_positions: u64,
    #[serde(with = "u64_string")]
    pub unwind_grace_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvVaultClosing {
    pub vault_id: ObjectId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvVaultClosed {
    pub vault_id: ObjectId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvDepositsPaused {
    pub vault_id: ObjectId,
    pub paused: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvMmReleaseToggled {
    pub vault_id: ObjectId,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvCuratorRotated {
    pub vault_id: ObjectId,
    pub old_cap_id: ObjectId,
    pub new_cap_id: ObjectId,
    pub recipient: SuiAddress,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvDeposited {
    pub vault_id: ObjectId,
    pub depositor: SuiAddress,
    pub curator_cap: Option<ObjectId>,
    #[serde(with = "u64_string")]
    pub amount: u64,
    #[serde(with = "u128_string")]
    pub shares: u128,
    #[serde(with = "u128_string")]
    pub total_shares: u128,
    #[serde(with = "u64_string")]
    pub locked_until_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvWithdrawRequested {
    pub vault_id: ObjectId,
    #[serde(with = "u64_string")]
    pub seq: u64,
    pub recipient: SuiAddress,
    pub curator_cap: Option<ObjectId>,
    #[serde(with = "u128_string")]
    pub shares: u128,
    #[serde(with = "u64_string")]
    pub basis: u64,
    #[serde(with = "u64_string")]
    pub requested_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvWithdrawFulfilled {
    pub vault_id: ObjectId,
    #[serde(with = "u64_string")]
    pub seq: u64,
    pub recipient: SuiAddress,
    #[serde(with = "u128_string")]
    pub shares: u128,
    #[serde(with = "u64_string")]
    pub value: u64,
    #[serde(with = "u64_string")]
    pub basis: u64,
    #[serde(with = "u64_string")]
    pub profit: u64,
    #[serde(with = "u64_string")]
    pub gross_fee: u64,
    #[serde(with = "u64_string")]
    pub protocol_cut: u64,
    #[serde(with = "u64_string")]
    pub curator_net: u64,
    #[serde(with = "u128_string")]
    pub curator_shares_minted: u128,
    #[serde(with = "u64_string")]
    pub payout: u64,
    #[serde(with = "u128_string")]
    pub total_shares: u128,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvSessionSettled {
    pub vault_id: ObjectId,
    pub adapter: AssetType,
    pub forced: bool,
    pub taken: TvTypeAmountMap,
    pub returned: TvTypeAmountMap,
    #[serde(with = "u64_string")]
    pub positions_added: u64,
    #[serde(with = "u64_string")]
    pub positions_removed: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvPositionStored {
    pub vault_id: ObjectId,
    pub adapter: AssetType,
    pub position_id: ObjectId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvPositionRemoved {
    pub vault_id: ObjectId,
    pub adapter: AssetType,
    pub position_id: ObjectId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvAdapterAllowed {
    pub adapter: AssetType,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvAdapterDisallowed {
    pub adapter: AssetType,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvOracleAllowed {
    pub oracle: AssetType,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvOracleDisallowed {
    pub oracle: AssetType,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvProtocolConfigUpdated {
    #[serde(with = "u64_string")]
    pub min_curator_share_bps: u64,
    pub enforce_curator_share: bool,
    #[serde(with = "u64_string")]
    pub max_curator_fee_bps: u64,
    #[serde(with = "u64_string")]
    pub protocol_fee_bps: u64,
    #[serde(with = "u64_string")]
    pub max_price_age_ms: u64,
    pub paused: bool,
}

/// `trading_vault::vault_mm::CollateralReleased`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvCollateralReleased {
    pub vault_id: ObjectId,
    pub asset_type: AssetType,
    #[serde(with = "u64_string")]
    pub amount: u64,
    pub bucket_id: ObjectId,
    #[serde(with = "u64_string")]
    pub quote_nonce: u64,
    pub is_writer_flow: bool,
}

/// `deepbook_adapter::deepbook_adapter::CustodyCreated`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvCustodyCreated {
    pub vault_id: ObjectId,
    pub custody_id: ObjectId,
    pub balance_manager_id: ObjectId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvPoolAllowed {
    pub pool_id: ObjectId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvPoolDisallowed {
    pub pool_id: ObjectId,
}

/// `options_adapter::options_adapter::RfqOpened`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvRfqOpened {
    pub vault_id: ObjectId,
    pub ticket_id: ObjectId,
    pub auction_id: ObjectId,
    pub bucket_id: ObjectId,
    #[serde(with = "u64_string")]
    pub write_amount: u64,
    #[serde(with = "u64_string")]
    pub escrow_amount: u64,
    #[serde(with = "u64_string")]
    pub reserve_premium: u64,
    pub is_put: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvRfqSettled {
    pub vault_id: ObjectId,
    pub ticket_id: ObjectId,
    pub auction_id: ObjectId,
    pub bucket_id: ObjectId,
    pub filled: bool,
    #[serde(with = "u64_string")]
    pub net_premium: u64,
    #[serde(with = "u64_string")]
    pub fee: u64,
    pub position_id: Option<ObjectId>,
    pub is_put: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TvPositionRedeemed {
    pub vault_id: ObjectId,
    pub bucket_id: ObjectId,
    pub position_id: ObjectId,
    #[serde(with = "u64_string")]
    pub underlying_out: u64,
    #[serde(with = "u64_string")]
    pub settlement_out: u64,
    pub is_put: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum ChainEvent {
    BucketCreated(BucketCreated),
    WriteExecuted(WriteExecuted),
    Exercised(Exercised),
    Redeemed(Redeemed),
    ExpiredOptionBurned(ExpiredOptionBurned),
    BucketCleaned(BucketCleaned),
    BucketInvalidated(BucketInvalidated),
    BucketRevalidated(BucketRevalidated),
    SignerCreated(SignerCreated),
    SigningKeyRotated(SigningKeyRotated),
    FeeUpdated(FeeUpdated),
    TreasuryWithdrawn(TreasuryWithdrawn),
    DeepBookPoolCreated(DeepBookPoolCreated),
    DeepBookOrderFilled(DeepBookOrderFilled),
    CollateralizedWrite(CollateralizedWrite),
    AuctionCreated(AuctionCreated),
    AuctionBid(AuctionBid),
    AuctionSettled(AuctionSettled),
    AuctionUnfilled(AuctionUnfilled),
    RfqCreated(RfqCreated),
    RfqSettled(RfqSettled),
    RfqExpiredUnsold(RfqExpiredUnsold),
    VaultRfqSettled(VaultRfqSettled),
    VaultRfqUnsold(VaultRfqUnsold),
    SwapRfqSettled(SwapRfqSettled),
    SwapRfqUnfilled(SwapRfqUnfilled),
    VaultCreated(VaultCreated),
    VaultDeposit(VaultDeposit),
    SharesClaimed(SharesClaimed),
    WithdrawInitiated(WithdrawInitiated),
    WithdrawCompleted(WithdrawCompleted),
    InstantWithdraw(InstantWithdraw),
    VaultBucketSelected(VaultBucketSelected),
    VaultPositionRedeemed(VaultPositionRedeemed),
    VaultFeesCharged(VaultFeesCharged),
    VaultRoundFinalized(VaultRoundFinalized),
    VaultConfigUpdated(VaultConfigUpdated),
    VaultConfigApplied(VaultConfigApplied),
    VaultDepositsPaused(VaultDepositsPaused),
    // cash-secured puts
    PutBucketCreated(PutBucketCreated),
    PutWriteExecuted(PutWriteExecuted),
    PutCollateralizedWrite(PutCollateralizedWrite),
    PutExercised(PutExercised),
    PutRedeemed(PutRedeemed),
    PutExpiredOptionBurned(PutExpiredOptionBurned),
    PutBucketCleaned(PutBucketCleaned),
    PutBucketInvalidated(PutBucketInvalidated),
    PutBucketRevalidated(PutBucketRevalidated),
    PutRfqCreated(PutRfqCreated),
    PutRfqSettled(PutRfqSettled),
    PutRfqExpiredUnsold(PutRfqExpiredUnsold),
    // curated trading vaults (SO-282)
    TvVaultCreated(TvVaultCreated),
    TvVaultClosing(TvVaultClosing),
    TvVaultClosed(TvVaultClosed),
    TvDepositsPaused(TvDepositsPaused),
    TvMmReleaseToggled(TvMmReleaseToggled),
    TvCuratorRotated(TvCuratorRotated),
    TvDeposited(TvDeposited),
    TvWithdrawRequested(TvWithdrawRequested),
    TvWithdrawFulfilled(TvWithdrawFulfilled),
    TvSessionSettled(TvSessionSettled),
    TvPositionStored(TvPositionStored),
    TvPositionRemoved(TvPositionRemoved),
    TvAdapterAllowed(TvAdapterAllowed),
    TvAdapterDisallowed(TvAdapterDisallowed),
    TvOracleAllowed(TvOracleAllowed),
    TvOracleDisallowed(TvOracleDisallowed),
    TvProtocolConfigUpdated(TvProtocolConfigUpdated),
    TvCollateralReleased(TvCollateralReleased),
    TvCustodyCreated(TvCustodyCreated),
    TvPoolAllowed(TvPoolAllowed),
    TvPoolDisallowed(TvPoolDisallowed),
    TvRfqOpened(TvRfqOpened),
    TvRfqSettled(TvRfqSettled),
    TvPositionRedeemed(TvPositionRedeemed),
}

/// An envelope wrapping a `ChainEvent` with the ordering metadata the indexer
/// emits. The `sequence` is monotonic across the stream so consumers can
/// detect gaps and resume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedEvent {
    #[serde(with = "u64_string")]
    pub sequence: u64,
    #[serde(with = "u64_string")]
    pub timestamp_ms: u64,
    pub event: ChainEvent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_event_tagged_envelope() {
        let evt = ChainEvent::BucketCreated(BucketCreated {
            bucket_id: ObjectId::new([0x01; 32]),
            asset_type: AssetType::new("BTC"),
            settlement_type: AssetType::new("USDC"),
            call_type: AssetType::new("0x9::call_0::CALL_0"),
            expiry_ms: 1_748_534_400_000,
            strike: 50_000_000_000,
            strike_scale: 0,
        });
        let v: serde_json::Value = serde_json::to_value(&evt).unwrap();
        assert_eq!(v["type"], "BucketCreated");
        assert_eq!(v["payload"]["asset_type"], "BTC");
        assert_eq!(v["payload"]["strike"], "50000000000");
        assert_eq!(v["payload"]["strike_scale"], 0);

        let back: ChainEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back, evt);
    }

    #[test]
    fn strike_as_f64_applies_scale() {
        let ev = BucketCreated {
            bucket_id: ObjectId::new([0; 32]),
            asset_type: AssetType::new("DEEP"),
            settlement_type: AssetType::new("USDC"),
            call_type: AssetType::new("0x9::call_0::CALL_0"),
            expiry_ms: 0,
            strike: 15_000,
            strike_scale: 5,
        };
        // scale=5 → divisor 100_000 → 15_000 / 100_000 = 0.15
        assert!((ev.strike_as_f64() - 0.15).abs() < 1e-12);
    }

    #[test]
    fn indexed_envelope_round_trips() {
        let env = IndexedEvent {
            sequence: 42,
            timestamp_ms: 1,
            event: ChainEvent::FeeUpdated(FeeUpdated { old_bps: 0, new_bps: 50 }),
        };
        let j = serde_json::to_string(&env).unwrap();
        let back: IndexedEvent = serde_json::from_str(&j).unwrap();
        assert_eq!(back, env);
    }
}
