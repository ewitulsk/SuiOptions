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
use protocol_types::events::{ChainEvent, IndexedEvent};
use protocol_types::ids::{ObjectId, SuiAddress};

use crate::store::{AccountState, BucketState, PositionState};

use super::schema::{
    account_balances, accounts, buckets, indexed_events, indexer_progress, positions,
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

impl IndexedEventRow {
    /// Reconstruct an `IndexedEvent` from a stored row. Returns the same shape
    /// `Store::ingest` produces, so the fanout can serve historical reads
    /// indistinguishably from live ones.
    pub fn into_indexed_event(self) -> anyhow::Result<IndexedEvent> {
        let event: ChainEvent = serde_json::from_value(self.payload).map_err(|e| {
            anyhow::anyhow!("decoding payload for sequence {}: {e}", self.sequence)
        })?;
        Ok(IndexedEvent {
            sequence: self.sequence as u64,
            timestamp_ms: self.timestamp_ms as u64,
            event,
        })
    }
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
        ChainEvent::AccountCreated(_) => "AccountCreated",
        ChainEvent::AccountDeposit(_) => "AccountDeposit",
        ChainEvent::AccountWithdraw(_) => "AccountWithdraw",
        ChainEvent::SigningKeyRotated(_) => "SigningKeyRotated",
        ChainEvent::FeeUpdated(_) => "FeeUpdated",
        ChainEvent::TreasuryWithdrawn(_) => "TreasuryWithdrawn",
    }
}

// ---------- accounts / account_balances ----------

#[derive(Queryable, Identifiable, Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = accounts)]
#[diesel(primary_key(account_id))]
pub struct AccountRow {
    pub account_id: String,
    pub owner: Option<String>,
    pub signing_pubkey: Vec<u8>,
    pub updated_at_seq: i64,
}

#[derive(Queryable, Identifiable, Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = account_balances)]
#[diesel(primary_key(account_id, asset_type))]
pub struct AccountBalanceRow {
    pub account_id: String,
    pub asset_type: String,
    pub balance: BigDecimal,
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
    pub strike: BigDecimal,
    pub strike_scale: i16,
    pub expiry_ms: i64,
    pub total_written: BigDecimal,
    pub exercise_cursor: BigDecimal,
    pub cleaned: bool,
    pub invalidated: bool,
    pub updated_at_seq: i64,
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
                strike: bigdecimal_to_u128(&self.strike)?,
                strike_scale: u8::try_from(self.strike_scale).map_err(|_| {
                    anyhow::anyhow!("strike_scale out of u8 range: {}", self.strike_scale)
                })?,
                expiry_ms: self.expiry_ms as u64,
                total_written: bigdecimal_to_u128(&self.total_written)?,
                exercise_cursor: bigdecimal_to_u128(&self.exercise_cursor)?,
                cleaned: self.cleaned,
                invalidated: self.invalidated,
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
            },
        ))
    }
}

/// Helper used by `repo::hydrate_views` to fold per-asset balance rows into
/// the `AccountState.balances` BTreeMap on the matching account.
pub fn account_row_into_state(row: AccountRow) -> anyhow::Result<(ObjectId, AccountState)> {
    let id = ObjectId::from_hex(&row.account_id)
        .map_err(|e| anyhow::anyhow!("account_id {}: {e}", row.account_id))?;
    let owner = row
        .owner
        .as_deref()
        .map(SuiAddress::from_hex)
        .transpose()
        .map_err(|e| anyhow::anyhow!("owner {:?}: {e}", row.owner))?;
    Ok((
        id,
        AccountState {
            owner,
            signing_pubkey: row.signing_pubkey,
            balances: Default::default(),
        },
    ))
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
