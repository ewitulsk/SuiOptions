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
    pub signer_account_id: ObjectId,
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
pub struct AccountCreated {
    pub account_id: ObjectId,
    pub owner: SuiAddress,
    /// Tag for the registered signing key. BCS-encodes as a single u8;
    /// must match the on-chain struct field order in `events.move`.
    pub signing_scheme: crate::SigningScheme,
    #[serde(with = "crate::coding::bytes_hex")]
    pub signing_pubkey: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountDeposit {
    pub account_id: ObjectId,
    pub asset_type: AssetType,
    #[serde(with = "u64_string")]
    pub amount: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountWithdraw {
    pub account_id: ObjectId,
    pub asset_type: AssetType,
    #[serde(with = "u64_string")]
    pub amount: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigningKeyRotated {
    pub account_id: ObjectId,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RfqCreated {
    pub rfq_id: ObjectId,
    pub bucket_id: ObjectId,
    /// Vault ID, or seller address-as-ID for standalone auctions.
    pub origin: ObjectId,
    #[serde(with = "u64_string")]
    pub amount: u64,
    #[serde(with = "u64_string")]
    pub reserve_premium: u64,
    #[serde(with = "u64_string")]
    pub deadline_ms: u64,
    #[serde(with = "u64_string")]
    pub max_deadline_ms: u64,
    #[serde(with = "u64_string")]
    pub min_increment_bps: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RfqBid {
    pub rfq_id: ObjectId,
    pub bidder: SuiAddress,
    pub call_recipient: SuiAddress,
    #[serde(with = "u64_string")]
    pub premium: u64,
    /// 0 if this was the first bid.
    #[serde(with = "u64_string")]
    pub previous_premium: u64,
    /// Post-anti-snipe deadline.
    #[serde(with = "u64_string")]
    pub new_deadline_ms: u64,
}

/// Mirrors `WriteExecuted`'s economic fields so the positions
/// materializer can treat both as "a position was minted with premium X".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RfqSettled {
    pub rfq_id: ObjectId,
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
    pub bucket_id: ObjectId,
    pub origin: ObjectId,
    #[serde(with = "u64_string")]
    pub amount: u64,
    #[serde(with = "u64_string")]
    pub reserve_premium: u64,
}

// ─── proceeds-swap auction (swap_auction.move) ───

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapRfqCreated {
    pub swap_id: ObjectId,
    /// Originating vault ID.
    pub origin: ObjectId,
    #[serde(with = "u64_string")]
    pub amount_s: u64,
    #[serde(with = "u64_string")]
    pub reserve_underlying: u64,
    #[serde(with = "u64_string")]
    pub deadline_ms: u64,
    #[serde(with = "u64_string")]
    pub max_deadline_ms: u64,
    #[serde(with = "u64_string")]
    pub min_increment_bps: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapRfqBid {
    pub swap_id: ObjectId,
    pub bidder: SuiAddress,
    /// Underlying offered by this bid.
    #[serde(with = "u64_string")]
    pub underlying: u64,
    /// 0 if this was the first bid.
    #[serde(with = "u64_string")]
    pub previous_underlying: u64,
    /// Post-anti-snipe deadline.
    #[serde(with = "u64_string")]
    pub new_deadline_ms: u64,
}

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

/// Tagged union over every event the indexer may publish.
///
/// The variant name is what shows up as `"type"` over the wire; the payload
/// rides under `"payload"`. This is the same envelope shape as the WS
/// retail/MM messages so a generic event reader can treat both alike.
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
    AccountCreated(AccountCreated),
    AccountDeposit(AccountDeposit),
    AccountWithdraw(AccountWithdraw),
    SigningKeyRotated(SigningKeyRotated),
    FeeUpdated(FeeUpdated),
    TreasuryWithdrawn(TreasuryWithdrawn),
    DeepBookPoolCreated(DeepBookPoolCreated),
    DeepBookOrderFilled(DeepBookOrderFilled),
    CollateralizedWrite(CollateralizedWrite),
    RfqCreated(RfqCreated),
    RfqBid(RfqBid),
    RfqSettled(RfqSettled),
    RfqExpiredUnsold(RfqExpiredUnsold),
    SwapRfqCreated(SwapRfqCreated),
    SwapRfqBid(SwapRfqBid),
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
