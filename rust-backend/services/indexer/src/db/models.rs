//! Diesel row types + conversions to/from the in-memory protocol types.
//!
//! The protocol types use `u64` / `u128`; Postgres NUMERIC maps to
//! `bigdecimal::BigDecimal`. Conversions happen here so `repo.rs` and the
//! worker keep their domain types.

use bigdecimal::BigDecimal;
use bigdecimal::ToPrimitive;
use chrono::{DateTime, Utc};
use diesel::prelude::*;
use std::str::FromStr;

use protocol_types::asset::AssetType;
use protocol_types::events::ChainEvent;
use protocol_types::ids::{ObjectId, SuiAddress};

use crate::store::{
    AccountState, BucketState, DeepBookPoolState, ExchangeMarketState, PositionState,
    ReceiptState, RfqState, RfqStatus, TradingVaultPositionState, TradingVaultState,
    VaultRoundState, VaultState,
};

use super::schema::{
    accounts, bucket_deepbook_pools, buckets, event_participants, exchange_market_links,
    indexed_events, indexer_progress, positions, rfq_bids, rfqs, trading_vault_positions,
    trading_vaults, vault_rounds, vault_user_receipts, vaults,
};

// ---------- indexer_progress ----------

#[derive(Queryable, Identifiable, Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = indexer_progress)]
#[diesel(primary_key(id))]
pub struct ProgressRow {
    pub id: i16,
    pub last_checkpoint: i64,
    pub last_sequence: i64,
    pub updated_at: DateTime<Utc>,
}

// ---------- indexed_events ----------

#[derive(Queryable, Identifiable, Debug, Clone)]
#[diesel(table_name = indexed_events)]
#[diesel(primary_key(sequence))]
pub struct IndexedEventRow {
    pub sequence: i64,
    pub checkpoint: i64,
    pub tx_digest: String,
    pub event_index: i32,
    pub timestamp_ms: i64,
    pub event_type: String,
    pub payload: serde_json::Value,
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = indexed_events)]
pub struct NewIndexedEventRow {
    pub sequence: i64,
    pub checkpoint: i64,
    pub tx_digest: String,
    pub event_index: i32,
    pub timestamp_ms: i64,
    pub event_type: String,
    pub payload: serde_json::Value,
}

// ---------- event_participants ----------

/// One (event, address, role) edge — every address an event touches, so the
/// generalized `events` query can filter by "involves address X in any role"
/// without OR-ing across payload keys. Insert-only.
#[derive(Queryable, Insertable, Debug, Clone)]
#[diesel(table_name = event_participants)]
pub struct EventParticipantRow {
    pub sequence: i64,
    pub address: String,
    pub role: String,
}

/// Tag used in `indexed_events.event_type`. Stable identifiers — they're
/// what downstream consumers grep on and they index well.
pub fn event_type_tag(ev: &ChainEvent) -> &'static str {
    match ev {
        ChainEvent::BucketCreated(_) => "BucketCreated",
        ChainEvent::WriteExecuted(_) => "WriteExecuted",
        ChainEvent::Exercised(_) => "Exercised",
        ChainEvent::Redeemed(_) => "Redeemed",
        ChainEvent::ExpiredOptionBurned(_) => "ExpiredOptionBurned",
        ChainEvent::BucketCleaned(_) => "BucketCleaned",
        ChainEvent::BucketInvalidated(_) => "BucketInvalidated",
        ChainEvent::BucketRevalidated(_) => "BucketRevalidated",
        ChainEvent::SignerCreated(_) => "SignerCreated",
        ChainEvent::SigningKeyRotated(_) => "SigningKeyRotated",
        ChainEvent::FeeUpdated(_) => "FeeUpdated",
        ChainEvent::TreasuryWithdrawn(_) => "TreasuryWithdrawn",
        ChainEvent::DeepBookPoolCreated(_) => "DeepBookPoolCreated",
        ChainEvent::DeepBookOrderFilled(_) => "DeepBookOrderFilled",
        ChainEvent::CollateralizedWrite(_) => "CollateralizedWrite",
        ChainEvent::AuctionCreated(_) => "AuctionCreated",
        ChainEvent::AuctionBid(_) => "AuctionBid",
        ChainEvent::AuctionSettled(_) => "AuctionSettled",
        ChainEvent::AuctionUnfilled(_) => "AuctionUnfilled",
        ChainEvent::RfqCreated(_) => "RfqCreated",
        ChainEvent::RfqSettled(_) => "RfqSettled",
        ChainEvent::RfqExpiredUnsold(_) => "RfqExpiredUnsold",
        ChainEvent::VaultRfqSettled(_) => "VaultRfqSettled",
        ChainEvent::VaultRfqUnsold(_) => "VaultRfqUnsold",
        ChainEvent::SwapRfqSettled(_) => "SwapRfqSettled",
        ChainEvent::SwapRfqUnfilled(_) => "SwapRfqUnfilled",
        ChainEvent::VaultCreated(_) => "VaultCreated",
        ChainEvent::VaultDeposit(_) => "VaultDeposit",
        ChainEvent::SharesClaimed(_) => "SharesClaimed",
        ChainEvent::WithdrawInitiated(_) => "WithdrawInitiated",
        ChainEvent::WithdrawCompleted(_) => "WithdrawCompleted",
        ChainEvent::InstantWithdraw(_) => "InstantWithdraw",
        ChainEvent::VaultBucketSelected(_) => "VaultBucketSelected",
        ChainEvent::VaultPositionRedeemed(_) => "VaultPositionRedeemed",
        ChainEvent::VaultFeesCharged(_) => "VaultFeesCharged",
        ChainEvent::VaultRoundFinalized(_) => "VaultRoundFinalized",
        ChainEvent::VaultConfigUpdated(_) => "VaultConfigUpdated",
        ChainEvent::VaultConfigApplied(_) => "VaultConfigApplied",
        ChainEvent::VaultDepositsPaused(_) => "VaultDepositsPaused",
        ChainEvent::PutBucketCreated(_) => "PutBucketCreated",
        ChainEvent::PutWriteExecuted(_) => "PutWriteExecuted",
        ChainEvent::PutCollateralizedWrite(_) => "PutCollateralizedWrite",
        ChainEvent::PutExercised(_) => "PutExercised",
        ChainEvent::PutRedeemed(_) => "PutRedeemed",
        ChainEvent::PutExpiredOptionBurned(_) => "PutExpiredOptionBurned",
        ChainEvent::PutBucketCleaned(_) => "PutBucketCleaned",
        ChainEvent::PutBucketInvalidated(_) => "PutBucketInvalidated",
        ChainEvent::PutBucketRevalidated(_) => "PutBucketRevalidated",
        ChainEvent::PutRfqCreated(_) => "PutRfqCreated",
        ChainEvent::PutRfqSettled(_) => "PutRfqSettled",
        ChainEvent::PutRfqExpiredUnsold(_) => "PutRfqExpiredUnsold",
        ChainEvent::OffsetClosed(_) => "OffsetClosed",
        ChainEvent::SpreadWritten(_) => "SpreadWritten",
        ChainEvent::SpreadUnwound(_) => "SpreadUnwound",
        ChainEvent::SpreadClosed(_) => "SpreadClosed",
        ChainEvent::SpreadRedeemed(_) => "SpreadRedeemed",
        ChainEvent::TvVaultCreated(_) => "TvVaultCreated",
        ChainEvent::TvVaultClosing(_) => "TvVaultClosing",
        ChainEvent::TvVaultClosed(_) => "TvVaultClosed",
        ChainEvent::TvDepositsPaused(_) => "TvDepositsPaused",
        ChainEvent::TvMmReleaseToggled(_) => "TvMmReleaseToggled",
        ChainEvent::TvCuratorRotated(_) => "TvCuratorRotated",
        ChainEvent::TvDeposited(_) => "TvDeposited",
        ChainEvent::TvWithdrawRequested(_) => "TvWithdrawRequested",
        ChainEvent::TvWithdrawFulfilled(_) => "TvWithdrawFulfilled",
        ChainEvent::TvDepositAssetAdded(_) => "TvDepositAssetAdded",
        ChainEvent::TvDepositAssetRemoved(_) => "TvDepositAssetRemoved",
        ChainEvent::TvHaircutsSet(_) => "TvHaircutsSet",
        ChainEvent::TvPayoutAssetAmended(_) => "TvPayoutAssetAmended",
        ChainEvent::TvSessionSettled(_) => "TvSessionSettled",
        ChainEvent::TvPositionStored(_) => "TvPositionStored",
        ChainEvent::TvPositionRemoved(_) => "TvPositionRemoved",
        ChainEvent::TvPositionAppraised(_) => "TvPositionAppraised",
        ChainEvent::TvVaultAppraised(_) => "TvVaultAppraised",
        ChainEvent::TvAdapterAllowed(_) => "TvAdapterAllowed",
        ChainEvent::TvAdapterDisallowed(_) => "TvAdapterDisallowed",
        ChainEvent::TvOracleAllowed(_) => "TvOracleAllowed",
        ChainEvent::TvOracleDisallowed(_) => "TvOracleDisallowed",
        ChainEvent::TvProtocolConfigUpdated(_) => "TvProtocolConfigUpdated",
        ChainEvent::TvCollateralReleased(_) => "TvCollateralReleased",
        ChainEvent::TvCustodyCreated(_) => "TvCustodyCreated",
        ChainEvent::TvExchangeCustodyCreated(_) => "TvExchangeCustodyCreated",
        ChainEvent::TvVaultQuoteFilled(_) => "TvVaultQuoteFilled",
        ChainEvent::TvQuoteAdapterAdded(_) => "TvQuoteAdapterAdded",
        ChainEvent::TvQuoteAdapterRemoved(_) => "TvQuoteAdapterRemoved",
        ChainEvent::TvPoolAllowed(_) => "TvPoolAllowed",
        ChainEvent::TvPoolDisallowed(_) => "TvPoolDisallowed",
        ChainEvent::TvRfqOpened(_) => "TvRfqOpened",
        ChainEvent::TvRfqSettled(_) => "TvRfqSettled",
        ChainEvent::TvPositionRedeemed(_) => "TvPositionRedeemed",
        ChainEvent::TvMmCoinExercised(_) => "TvMmCoinExercised",
        ChainEvent::TvMmOffsetClosed(_) => "TvMmOffsetClosed",
        ChainEvent::TvMmCoinReleased(_) => "TvMmCoinReleased",
        ChainEvent::TvTakerSwapExecuted(_) => "TvTakerSwapExecuted",
        ChainEvent::TvBidPlaced(_) => "TvBidPlaced",
        ChainEvent::TvBidReclaimed(_) => "TvBidReclaimed",
        ChainEvent::TvBidRedeemed(_) => "TvBidRedeemed",
        ChainEvent::TvExternalAccountSet(_) => "TvExternalAccountSet",
        ChainEvent::TvExternalAccountCleared(_) => "TvExternalAccountCleared",
        ChainEvent::TvExternalReleased(_) => "TvExternalReleased",
        ChainEvent::TvExternalReturned(_) => "TvExternalReturned",
        ChainEvent::EquityPosted(_) => "EquityPosted",
        ChainEvent::PutSpreadWritten(_) => "PutSpreadWritten",
        ChainEvent::PutSpreadExercised(_) => "PutSpreadExercised",
        ChainEvent::PutSpreadClosed(_) => "PutSpreadClosed",
        ChainEvent::PutSpreadRedeemed(_) => "PutSpreadRedeemed",
        ChainEvent::VolPosted(_) => "VolPosted",
        ChainEvent::OptionMarketListed(_) => "OptionMarketListed",
        ChainEvent::ExchangeOptionFill(_) => "ExchangeOptionFill",
    }
}

// ---------- accounts (QuoteSigner registry — no balances; core holds no
// MM funds under the collateral abstraction) ----------

#[derive(Queryable, Identifiable, Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = accounts)]
#[diesel(primary_key(account_id))]
pub struct AccountRow {
    pub account_id: String,
    pub owner: Option<String>,
    pub signing_pubkey: Vec<u8>,
    /// On-chain signing-scheme tag (0=Ed25519, 1=Secp256k1, 2=Secp256r1).
    /// Nullable for rows the backfill couldn't resolve. Field order matches
    /// the column order in `schema.rs` for `Queryable`.
    pub signing_scheme: Option<i16>,
    pub updated_at_seq: i64,
}

// ---------- buckets ----------

#[derive(Queryable, Identifiable, Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = buckets)]
#[diesel(primary_key(bucket_id))]
pub struct BucketRow {
    pub bucket_id: String,
    pub asset_type: String,
    pub settlement_type: String,
    pub call_type: String,
    pub strike: BigDecimal,
    pub strike_scale: i16,
    pub expiry_ms: i64,
    pub total_written: BigDecimal,
    pub exercise_cursor: BigDecimal,
    pub cleaned: bool,
    pub invalidated: bool,
    pub updated_at_seq: i64,
    /// "call" or "put" — shared-table discriminator (defaults to "call").
    pub option_kind: String,
}

// ---------- bucket_deepbook_pools ----------

/// One bucket's DeepBook trading venue (SO-152). Insert-only with first-pool-
/// wins semantics (`ON CONFLICT DO NOTHING` on both bucket_id and pool_id).
#[derive(Queryable, Identifiable, Insertable, Debug, Clone)]
#[diesel(table_name = bucket_deepbook_pools)]
#[diesel(primary_key(bucket_id))]
pub struct DeepBookPoolRow {
    pub bucket_id: String,
    pub pool_id: String,
    pub base_asset_type: String,
    pub quote_asset_type: String,
    pub tick_size: i64,
    pub lot_size: i64,
    pub min_size: i64,
    pub taker_fee: i64,
    pub maker_fee: i64,
    pub created_checkpoint: i64,
    pub created_timestamp_ms: i64,
    pub updated_at_seq: i64,
}

impl DeepBookPoolRow {
    pub fn into_state(self) -> anyhow::Result<(ObjectId, DeepBookPoolState)> {
        let bucket = ObjectId::from_hex(&self.bucket_id)
            .map_err(|e| anyhow::anyhow!("deepbook bucket_id {}: {e}", self.bucket_id))?;
        let pool = ObjectId::from_hex(&self.pool_id)
            .map_err(|e| anyhow::anyhow!("deepbook pool_id {}: {e}", self.pool_id))?;
        Ok((
            bucket,
            DeepBookPoolState {
                pool_id: pool,
                base_asset_type: AssetType::new(self.base_asset_type),
                quote_asset_type: AssetType::new(self.quote_asset_type),
                tick_size: self.tick_size as u64,
                lot_size: self.lot_size as u64,
                min_size: self.min_size as u64,
                taker_fee: self.taker_fee as u64,
                maker_fee: self.maker_fee as u64,
            },
        ))
    }
}

// ---------- exchange_market_links ----------

/// One bucket's in-house exchange option market (SO-416). Insert-only with
/// first-listing-wins semantics (`ON CONFLICT DO NOTHING` on both bucket_id
/// and registry_id).
#[derive(Queryable, Identifiable, Insertable, Debug, Clone)]
#[diesel(table_name = exchange_market_links)]
#[diesel(primary_key(bucket_id))]
pub struct ExchangeMarketLinkRow {
    pub bucket_id: String,
    pub registry_id: String,
    pub is_put: bool,
    pub updated_at_seq: i64,
}

impl ExchangeMarketLinkRow {
    pub fn into_state(self) -> anyhow::Result<(ObjectId, ExchangeMarketState)> {
        let bucket = ObjectId::from_hex(&self.bucket_id)
            .map_err(|e| anyhow::anyhow!("exchange market bucket_id {}: {e}", self.bucket_id))?;
        let registry = ObjectId::from_hex(&self.registry_id)
            .map_err(|e| anyhow::anyhow!("exchange market registry_id {}: {e}", self.registry_id))?;
        Ok((bucket, ExchangeMarketState { registry_id: registry, is_put: self.is_put }))
    }
}

// ---------- positions ----------

#[derive(Queryable, Identifiable, Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = positions)]
#[diesel(primary_key(bucket_id, range_start))]
pub struct PositionRow {
    pub bucket_id: String,
    pub range_start: BigDecimal,
    pub range_end: BigDecimal,
    pub object_id: String,
    pub recipient: String,
    pub updated_at_seq: i64,
    /// SO-97 provenance, denormalized from the minting `WriteExecuted` so the
    /// GraphQL positions query needs only a positions×buckets join. Set once
    /// at mint; ignored by `into_state` (the in-memory view doesn't need it).
    pub premium_received: BigDecimal,
    pub mm_account_id: String,
    pub tx_digest: String,
    pub minted_at_ms: i64,
    /// "call" or "put" — shared-table discriminator (defaults to "call").
    pub option_kind: String,
}

// ---------- conversions to/from in-memory state ----------

impl BucketRow {
    pub fn into_state(self) -> anyhow::Result<(ObjectId, BucketState)> {
        let id = ObjectId::from_hex(&self.bucket_id)
            .map_err(|e| anyhow::anyhow!("bucket_id {}: {e}", self.bucket_id))?;
        Ok((
            id,
            BucketState {
                asset_type: AssetType::new(self.asset_type),
                settlement_type: AssetType::new(self.settlement_type),
                call_type: AssetType::new(self.call_type),
                strike: bigdecimal_to_u128(&self.strike)?,
                strike_scale: u8::try_from(self.strike_scale).map_err(|_| {
                    anyhow::anyhow!("strike_scale out of u8 range: {}", self.strike_scale)
                })?,
                expiry_ms: self.expiry_ms as u64,
                total_written: bigdecimal_to_u128(&self.total_written)?,
                exercise_cursor: bigdecimal_to_u128(&self.exercise_cursor)?,
                cleaned: self.cleaned,
                invalidated: self.invalidated,
                option_kind: self.option_kind,
            },
        ))
    }
}

impl PositionRow {
    pub fn into_state(self) -> anyhow::Result<((ObjectId, u128), PositionState)> {
        let bucket = ObjectId::from_hex(&self.bucket_id)
            .map_err(|e| anyhow::anyhow!("bucket_id {}: {e}", self.bucket_id))?;
        let object_id = ObjectId::from_hex(&self.object_id)
            .map_err(|e| anyhow::anyhow!("object_id {}: {e}", self.object_id))?;
        let recipient = SuiAddress::from_hex(&self.recipient)
            .map_err(|e| anyhow::anyhow!("recipient {}: {e}", self.recipient))?;
        let start = bigdecimal_to_u128(&self.range_start)?;
        let end = bigdecimal_to_u128(&self.range_end)?;
        Ok((
            (bucket, start),
            PositionState {
                bucket_id: bucket,
                object_id,
                recipient,
                range_start: start,
                range_end: end,
                option_kind: self.option_kind,
            },
        ))
    }
}

/// Row → in-memory QuoteSigner registration, used by `repo::hydrate_views`.
pub fn account_row_into_state(row: AccountRow) -> anyhow::Result<(ObjectId, AccountState)> {
    let id = ObjectId::from_hex(&row.account_id)
        .map_err(|e| anyhow::anyhow!("account_id {}: {e}", row.account_id))?;
    let owner = row
        .owner
        .as_deref()
        .map(SuiAddress::from_hex)
        .transpose()
        .map_err(|e| anyhow::anyhow!("owner {:?}: {e}", row.owner))?;
    let signing_scheme = row
        .signing_scheme
        .map(|s| {
            u8::try_from(s)
                .ok()
                .and_then(|b| protocol_types::SigningScheme::from_u8(b).ok())
                .ok_or_else(|| anyhow::anyhow!("invalid signing_scheme {s} for {}", row.account_id))
        })
        .transpose()?;
    Ok((
        id,
        AccountState {
            owner,
            signing_pubkey: row.signing_pubkey,
            signing_scheme,
        },
    ))
}

// ---------- rfqs / rfq_bids (C3) ----------

#[derive(Queryable, Identifiable, Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = rfqs)]
#[diesel(primary_key(rfq_id))]
pub struct RfqRow {
    /// The generic auction object id (rows are keyed by auction now).
    pub rfq_id: String,
    /// Null for swaps and for coupled option auctions not yet enriched.
    pub bucket_id: Option<String>,
    pub origin: String,
    pub amount: BigDecimal,
    pub reserve_premium: BigDecimal,
    pub deadline_ms: i64,
    pub best_premium: Option<BigDecimal>,
    pub best_bidder: Option<String>,
    pub status: String,
    pub winner: Option<String>,
    pub net_premium: Option<BigDecimal>,
    pub position_id: Option<String>,
    pub gross_premium: Option<BigDecimal>,
    pub fee: Option<BigDecimal>,
    pub updated_at_seq: i64,
    /// "call" | "put" | "swap" | "unknown" — what the auction is for.
    pub auction_kind: String,
    /// The options_rfq adapter's Rfq metadata object id; null for
    /// vault-coupled and swap auctions.
    pub meta_id: Option<String>,
}

impl RfqRow {
    pub fn into_state(self) -> anyhow::Result<(ObjectId, RfqState)> {
        let id = ObjectId::from_hex(&self.rfq_id)
            .map_err(|e| anyhow::anyhow!("rfq_id {}: {e}", self.rfq_id))?;
        let opt_addr = |s: &Option<String>| -> anyhow::Result<Option<SuiAddress>> {
            s.as_deref()
                .map(SuiAddress::from_hex)
                .transpose()
                .map_err(|e| anyhow::anyhow!("rfq {} address: {e}", self.rfq_id))
        };
        let opt_id = |s: &Option<String>, field: &str| -> anyhow::Result<Option<ObjectId>> {
            s.as_deref()
                .map(ObjectId::from_hex)
                .transpose()
                .map_err(|e| anyhow::anyhow!("rfq {field}: {e}"))
        };
        Ok((
            id,
            RfqState {
                meta_id: opt_id(&self.meta_id, "meta_id")?,
                bucket_id: opt_id(&self.bucket_id, "bucket_id")?,
                origin: ObjectId::from_hex(&self.origin)
                    .map_err(|e| anyhow::anyhow!("rfq origin {}: {e}", self.origin))?,
                amount: bigdecimal_to_u64(&self.amount)?,
                reserve_premium: bigdecimal_to_u64(&self.reserve_premium)?,
                deadline_ms: self.deadline_ms as u64,
                best_premium: self.best_premium.as_ref().map(bigdecimal_to_u64).transpose()?,
                best_bidder: opt_addr(&self.best_bidder)?,
                status: RfqStatus::from_str_tag(&self.status)?,
                winner: opt_addr(&self.winner)?,
                net_premium: self.net_premium.as_ref().map(bigdecimal_to_u64).transpose()?,
                position_id: opt_id(&self.position_id, "position_id")?,
                gross_premium: self.gross_premium.as_ref().map(bigdecimal_to_u64).transpose()?,
                fee: self.fee.as_ref().map(bigdecimal_to_u64).transpose()?,
                auction_kind: self.auction_kind,
            },
        ))
    }
}

#[derive(Queryable, Insertable, Debug, Clone)]
#[diesel(table_name = rfq_bids)]
pub struct RfqBidRow {
    /// The generic auction object id (matches `rfqs.rfq_id`).
    pub rfq_id: String,
    pub sequence: i64,
    pub bidder: String,
    /// `AuctionBid.token_recipient` — the option-coin recipient for option
    /// auctions (shared column name kept for schema stability).
    pub call_recipient: String,
    pub premium: BigDecimal,
    /// "call" | "put" | "swap" | "unknown", copied from the parent auction.
    pub auction_kind: String,
}

// ---------- vaults / vault_rounds / vault_user_receipts (D2) ----------

#[derive(Queryable, Identifiable, Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = vaults)]
#[diesel(primary_key(vault_id))]
pub struct VaultRow {
    pub vault_id: String,
    pub underlying_type: String,
    pub settlement_type: String,
    pub share_type: String,
    pub round: i64,
    pub current_bucket: Option<String>,
    pub latest_pps: Option<BigDecimal>,
    pub total_shares: BigDecimal,
    pub pending_deposits: BigDecimal,
    pub deposits_paused: bool,
    pub updated_at_seq: i64,
    pub mgmt_fee_bps_annual: Option<i64>,
    pub perf_fee_bps: Option<i64>,
    pub round_ms: Option<i64>,
    pub selling_window_ms: Option<i64>,
    pub min_strike_bps_over_spot: Option<i64>,
    pub max_strike_bps_over_spot: Option<i64>,
}

impl VaultRow {
    pub fn into_state(self) -> anyhow::Result<(ObjectId, VaultState)> {
        let id = ObjectId::from_hex(&self.vault_id)
            .map_err(|e| anyhow::anyhow!("vault_id {}: {e}", self.vault_id))?;
        Ok((
            id,
            VaultState {
                underlying_type: AssetType::new(self.underlying_type),
                settlement_type: AssetType::new(self.settlement_type),
                share_type: AssetType::new(self.share_type),
                round: self.round as u64,
                current_bucket: self
                    .current_bucket
                    .as_deref()
                    .map(ObjectId::from_hex)
                    .transpose()
                    .map_err(|e| anyhow::anyhow!("vault current_bucket: {e}"))?,
                latest_pps: self.latest_pps.as_ref().map(bigdecimal_to_u128).transpose()?,
                total_shares: bigdecimal_to_u64(&self.total_shares)?,
                pending_deposits: bigdecimal_to_u64(&self.pending_deposits)?,
                deposits_paused: self.deposits_paused,
                mgmt_fee_bps_annual: self.mgmt_fee_bps_annual.map(|x| x as u64),
                perf_fee_bps: self.perf_fee_bps.map(|x| x as u64),
                round_ms: self.round_ms.map(|x| x as u64),
                selling_window_ms: self.selling_window_ms.map(|x| x as u64),
                min_strike_bps_over_spot: self.min_strike_bps_over_spot.map(|x| x as u64),
                max_strike_bps_over_spot: self.max_strike_bps_over_spot.map(|x| x as u64),
            },
        ))
    }
}

#[derive(Queryable, Identifiable, Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = vault_rounds)]
#[diesel(primary_key(vault_id, round))]
pub struct VaultRoundRow {
    pub vault_id: String,
    pub round: i64,
    pub bucket_id: Option<String>,
    pub strike: Option<BigDecimal>,
    pub strike_scale: Option<i16>,
    pub expiry_ms: Option<i64>,
    pub pps: Option<BigDecimal>,
    pub aum: Option<BigDecimal>,
    pub shares: Option<BigDecimal>,
    pub premium_collected: Option<BigDecimal>,
    pub mgmt_fee: Option<BigDecimal>,
    pub perf_fee: Option<BigDecimal>,
    pub finalized_at_ms: Option<i64>,
    pub updated_at_seq: i64,
}

impl VaultRoundRow {
    pub fn into_state(self) -> anyhow::Result<((ObjectId, u64), VaultRoundState)> {
        let id = ObjectId::from_hex(&self.vault_id)
            .map_err(|e| anyhow::anyhow!("round vault_id {}: {e}", self.vault_id))?;
        Ok((
            (id, self.round as u64),
            VaultRoundState {
                bucket_id: self
                    .bucket_id
                    .as_deref()
                    .map(ObjectId::from_hex)
                    .transpose()
                    .map_err(|e| anyhow::anyhow!("round bucket_id: {e}"))?,
                strike: self.strike.as_ref().map(bigdecimal_to_u128).transpose()?,
                strike_scale: self
                    .strike_scale
                    .map(|s| u8::try_from(s).map_err(|_| anyhow::anyhow!("strike_scale {s}")))
                    .transpose()?,
                expiry_ms: self.expiry_ms.map(|v| v as u64),
                pps: self.pps.as_ref().map(bigdecimal_to_u128).transpose()?,
                aum: self.aum.as_ref().map(bigdecimal_to_u64).transpose()?,
                shares: self.shares.as_ref().map(bigdecimal_to_u64).transpose()?,
                premium_collected: self
                    .premium_collected
                    .as_ref()
                    .map(bigdecimal_to_u64)
                    .transpose()?,
                mgmt_fee: self.mgmt_fee.as_ref().map(bigdecimal_to_u64).transpose()?,
                perf_fee: self.perf_fee.as_ref().map(bigdecimal_to_u64).transpose()?,
                finalized_at_ms: self.finalized_at_ms.map(|v| v as u64),
            },
        ))
    }
}

#[derive(Queryable, Identifiable, Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = vault_user_receipts)]
#[diesel(primary_key(vault_id, owner, round, kind))]
pub struct VaultReceiptRow {
    pub vault_id: String,
    pub owner: String,
    pub round: i64,
    pub kind: String,
    pub amount: BigDecimal,
    pub settled: BigDecimal,
    pub updated_at_seq: i64,
}

impl VaultReceiptRow {
    pub fn into_state(self) -> anyhow::Result<((ObjectId, String, u64, String), ReceiptState)> {
        let id = ObjectId::from_hex(&self.vault_id)
            .map_err(|e| anyhow::anyhow!("receipt vault_id {}: {e}", self.vault_id))?;
        Ok((
            (id, self.owner, self.round as u64, self.kind),
            ReceiptState {
                amount: bigdecimal_to_u64(&self.amount)?,
                settled: bigdecimal_to_u64(&self.settled)?,
            },
        ))
    }
}

// ---------- trading_vaults / trading_vault_positions (SO-282) ----------

#[derive(Queryable, Identifiable, Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = trading_vaults)]
#[diesel(primary_key(vault_id))]
pub struct TradingVaultRow {
    pub vault_id: String,
    /// The vault's unit of account (renamed from deposit_asset in SO-370:
    /// deposits may arrive in any allowlisted asset).
    pub accounting_asset: String,
    pub creator: String,
    /// Current curator wallet (updated on TvCuratorRotated).
    pub curator: String,
    pub curator_cap_id: String,
    /// "open" | "closing" | "closed".
    pub state: String,
    pub lockup_ms: i64,
    pub curator_fee_bps: i64,
    pub unwind_grace_ms: i64,
    pub deposits_paused: bool,
    pub mm_release_enabled: bool,
    pub total_shares: BigDecimal,
    pub position_count: i64,
    pub pending_withdrawals: i64,
    pub latest_pps_e12: Option<BigDecimal>,
    pub updated_at_seq: i64,
    pub updated_at_ms: i64,
    /// External MM account wallet (SO-299); null when none is set.
    pub external_account: Option<String>,
    /// Outstanding external exposure (post latest release/return).
    pub external_exposure: i64,
    /// Latest keeper-posted account equity (EquityPosted).
    pub latest_external_equity: Option<i64>,
    pub external_equity_updated_at_ms: Option<i64>,
    /// NAV from the latest consumed appraisal (TvVaultAppraised, SO-304).
    pub latest_nav: Option<BigDecimal>,
    pub nav_updated_at_ms: Option<i64>,
}

impl TradingVaultRow {
    pub fn into_state(self) -> anyhow::Result<(ObjectId, TradingVaultState)> {
        let id = ObjectId::from_hex(&self.vault_id)
            .map_err(|e| anyhow::anyhow!("trading vault_id {}: {e}", self.vault_id))?;
        Ok((
            id,
            TradingVaultState {
                accounting_asset: AssetType::new(self.accounting_asset),
                creator: SuiAddress::from_hex(&self.creator)
                    .map_err(|e| anyhow::anyhow!("trading vault creator {}: {e}", self.creator))?,
                curator: SuiAddress::from_hex(&self.curator)
                    .map_err(|e| anyhow::anyhow!("trading vault curator {}: {e}", self.curator))?,
                curator_cap_id: ObjectId::from_hex(&self.curator_cap_id).map_err(|e| {
                    anyhow::anyhow!("trading vault curator_cap_id {}: {e}", self.curator_cap_id)
                })?,
                state: self.state,
                lockup_ms: self.lockup_ms as u64,
                curator_fee_bps: self.curator_fee_bps as u64,
                unwind_grace_ms: self.unwind_grace_ms as u64,
                deposits_paused: self.deposits_paused,
                mm_release_enabled: self.mm_release_enabled,
                total_shares: bigdecimal_to_u128(&self.total_shares)?,
                position_count: self.position_count as u64,
                pending_withdrawals: self.pending_withdrawals as u64,
                latest_pps_e12: self
                    .latest_pps_e12
                    .as_ref()
                    .map(bigdecimal_to_u128)
                    .transpose()?,
                updated_at_ms: self.updated_at_ms as u64,
                external_account: self.external_account,
                external_exposure: self.external_exposure as u64,
                latest_external_equity: self.latest_external_equity.map(|v| v as u64),
                external_equity_updated_at_ms: self
                    .external_equity_updated_at_ms
                    .map(|v| v as u64),
                latest_nav: self.latest_nav.as_ref().map(bigdecimal_to_u128).transpose()?,
                nav_updated_at_ms: self.nav_updated_at_ms.map(|v| v as u64),
            },
        ))
    }
}

#[derive(Queryable, Identifiable, Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = trading_vault_positions)]
#[diesel(primary_key(vault_id, position_id))]
pub struct TradingVaultPositionRow {
    pub vault_id: String,
    pub position_id: String,
    pub adapter: String,
    /// false once TvPositionRemoved lands (rows are kept for history).
    pub active: bool,
    pub stored_at_ms: i64,
    pub removed_at_ms: Option<i64>,
    pub updated_at_seq: i64,
    /// Latest appraisal mark, accounting-asset units (TvPositionAppraised).
    pub last_value: Option<i64>,
    pub last_appraised_at_ms: Option<i64>,
}

impl TradingVaultPositionRow {
    pub fn into_state(
        self,
    ) -> anyhow::Result<((ObjectId, ObjectId), TradingVaultPositionState)> {
        let vault = ObjectId::from_hex(&self.vault_id)
            .map_err(|e| anyhow::anyhow!("tv position vault_id {}: {e}", self.vault_id))?;
        let position = ObjectId::from_hex(&self.position_id)
            .map_err(|e| anyhow::anyhow!("tv position_id {}: {e}", self.position_id))?;
        Ok((
            (vault, position),
            TradingVaultPositionState {
                adapter: AssetType::new(self.adapter),
                active: self.active,
                stored_at_ms: self.stored_at_ms as u64,
                removed_at_ms: self.removed_at_ms.map(|v| v as u64),
                last_value: self.last_value.map(|v| v as u64),
                last_appraised_at_ms: self.last_appraised_at_ms.map(|v| v as u64),
            },
        ))
    }
}

// ---------- numeric helpers ----------

pub fn u64_to_bigdecimal(v: u64) -> BigDecimal {
    BigDecimal::from(v)
}

pub fn u128_to_bigdecimal(v: u128) -> BigDecimal {
    // `BigDecimal::from` isn't impl'd for u128, but the decimal-string form is
    // unambiguous and BigDecimal parses it without losing precision.
    BigDecimal::from_str(&v.to_string()).expect("u128 decimal is always valid")
}

pub fn bigdecimal_to_u64(v: &BigDecimal) -> anyhow::Result<u64> {
    v.to_u64()
        .ok_or_else(|| anyhow::anyhow!("value {v} doesn't fit in u64"))
}

pub fn bigdecimal_to_u128(v: &BigDecimal) -> anyhow::Result<u128> {
    // bigdecimal exposes `to_u128` via the `ToPrimitive` blanket. It returns
    // None on out-of-range; we propagate as an error.
    v.to_u128()
        .ok_or_else(|| anyhow::anyhow!("value {v} doesn't fit in u128"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u128_round_trips_through_bigdecimal() {
        let big = u128::MAX;
        let bd = u128_to_bigdecimal(big);
        let back = bigdecimal_to_u128(&bd).unwrap();
        assert_eq!(back, big);
    }

    #[test]
    fn u64_round_trips_through_bigdecimal() {
        let v: u64 = 1_234_567_890_123;
        let bd = u64_to_bigdecimal(v);
        assert_eq!(bigdecimal_to_u64(&bd).unwrap(), v);
    }
}
