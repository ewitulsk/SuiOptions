//! Diesel row types. On-chain u64/u128 quantities map to Postgres NUMERIC
//! via `bigdecimal::BigDecimal`; ids are base58 TEXT.

use bigdecimal::{BigDecimal, ToPrimitive};
use chrono::{DateTime, Utc};
use diesel::prelude::*;
use std::str::FromStr;

use super::schema::{
    account_balances, accounts, auction_bids, auctions, buckets, event_participants,
    indexed_events, indexer_progress, positions, vault_receipts, vault_rounds, vaults,
};

// ---------- indexer_progress ----------

#[derive(Queryable, Identifiable, Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = indexer_progress)]
#[diesel(primary_key(id))]
pub struct ProgressRow {
    pub id: i16,
    pub last_slot: i64,
    pub finalized_slot: i64,
    pub updated_at: DateTime<Utc>,
}

// ---------- indexed_events ----------

#[derive(Queryable, Identifiable, Debug, Clone)]
#[diesel(table_name = indexed_events)]
#[diesel(primary_key(sequence))]
pub struct IndexedEventRow {
    pub sequence: i64,
    pub slot: i64,
    pub signature: String,
    pub tx_index: i64,
    pub inner_ix_index: i32,
    pub program: String,
    pub timestamp_ms: i64,
    pub event_type: String,
    pub payload: serde_json::Value,
}

/// Insert form — `sequence` is a BIGSERIAL assigned by Postgres.
#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = indexed_events)]
pub struct NewIndexedEventRow {
    pub slot: i64,
    pub signature: String,
    pub tx_index: i64,
    pub inner_ix_index: i32,
    pub program: String,
    pub timestamp_ms: i64,
    pub event_type: String,
    pub payload: serde_json::Value,
}

#[derive(Queryable, Insertable, Debug, Clone)]
#[diesel(table_name = event_participants)]
pub struct EventParticipantRow {
    pub sequence: i64,
    pub address: String,
    pub role: String,
}

// ---------- materialised views ----------

#[derive(Queryable, Identifiable, Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = accounts)]
#[diesel(primary_key(account_id))]
pub struct AccountRow {
    pub account_id: String,
    pub owner: String,
    pub signing_scheme: i16,
    pub signing_pubkey: Vec<u8>,
    pub updated_at_slot: i64,
}

#[derive(Queryable, Identifiable, Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = account_balances)]
#[diesel(primary_key(account_id, mint))]
pub struct AccountBalanceRow {
    pub account_id: String,
    pub mint: String,
    pub balance: BigDecimal,
    pub updated_at_slot: i64,
}

#[derive(Queryable, Identifiable, Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = buckets)]
#[diesel(primary_key(bucket_id))]
pub struct BucketRow {
    pub bucket_id: String,
    pub underlying_mint: String,
    pub settlement_mint: String,
    pub option_mint: String,
    /// "call" or "put".
    pub option_kind: String,
    pub strike: BigDecimal,
    pub strike_scale: i16,
    pub expiry_ms: i64,
    pub total_written: BigDecimal,
    pub exercise_cursor: BigDecimal,
    pub cleaned: bool,
    pub invalidated: bool,
    pub updated_at_slot: i64,
}

#[derive(Queryable, Identifiable, Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = positions)]
#[diesel(primary_key(position_id))]
pub struct PositionRow {
    pub position_id: String,
    pub bucket_id: String,
    pub range_start: BigDecimal,
    pub range_end: BigDecimal,
    pub recipient: String,
    /// "call" or "put".
    pub option_kind: String,
    pub premium_received: BigDecimal,
    pub mm_account_id: Option<String>,
    pub signature: String,
    pub minted_at_ms: i64,
    pub updated_at_slot: i64,
}

#[derive(Queryable, Identifiable, Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = auctions)]
#[diesel(primary_key(auction_id))]
pub struct AuctionRow {
    pub auction_id: String,
    /// swap | covered_call | cash_secured_put.
    pub mode: String,
    pub bucket_id: Option<String>,
    pub creator: String,
    pub escrow_mint: String,
    pub bid_mint: String,
    pub amount: BigDecimal,
    pub notional: BigDecimal,
    pub reserve_bid: BigDecimal,
    pub deadline_ms: i64,
    pub max_deadline_ms: i64,
    pub min_increment_bps: i64,
    pub settle_authority: Option<String>,
    pub best_bid: Option<BigDecimal>,
    pub best_bidder: Option<String>,
    /// open | settled | unsold.
    pub status: String,
    pub winner: Option<String>,
    pub token_recipient: Option<String>,
    pub position_id: Option<String>,
    pub gross_bid: Option<BigDecimal>,
    pub fee: Option<BigDecimal>,
    pub net_proceeds: Option<BigDecimal>,
    pub bid_refunded: Option<bool>,
    pub updated_at_slot: i64,
}

#[derive(Queryable, Insertable, Debug, Clone)]
#[diesel(table_name = auction_bids)]
pub struct AuctionBidRow {
    pub auction_id: String,
    pub sequence: i64,
    pub bidder: String,
    pub token_recipient: String,
    pub bid: BigDecimal,
    pub previous_bid: BigDecimal,
    pub deadline_ms: i64,
}

#[derive(Queryable, Identifiable, Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = vaults)]
#[diesel(primary_key(vault_id))]
pub struct VaultRow {
    pub vault_id: String,
    pub underlying_mint: String,
    pub settlement_mint: String,
    pub share_mint: String,
    pub round: i64,
    pub current_bucket: Option<String>,
    pub latest_pps: Option<BigDecimal>,
    pub total_shares: BigDecimal,
    pub pending_deposits: BigDecimal,
    pub deposits_paused: bool,
    pub mgmt_fee_bps_annual: Option<i64>,
    pub perf_fee_bps: Option<i64>,
    pub round_ms: Option<i64>,
    pub selling_window_ms: Option<i64>,
    pub min_strike_bps_over_spot: Option<i64>,
    pub max_strike_bps_over_spot: Option<i64>,
    pub updated_at_slot: i64,
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
    pub selling_ends_ms: Option<i64>,
    pub spot: Option<BigDecimal>,
    pub spot_scale: Option<i16>,
    pub pps: Option<BigDecimal>,
    pub aum: Option<BigDecimal>,
    pub shares: Option<BigDecimal>,
    pub premium_collected: Option<BigDecimal>,
    pub mgmt_fee: Option<BigDecimal>,
    pub perf_fee: Option<BigDecimal>,
    pub finalized_at_ms: Option<i64>,
    pub updated_at_slot: i64,
}

#[derive(Queryable, Identifiable, Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = vault_receipts)]
#[diesel(primary_key(vault_id, owner, round, kind))]
pub struct VaultReceiptRow {
    pub vault_id: String,
    pub owner: String,
    pub round: i64,
    /// deposit | withdraw.
    pub kind: String,
    pub amount: BigDecimal,
    pub settled: BigDecimal,
    pub updated_at_slot: i64,
}

// ---------- numeric helpers ----------

pub fn u64_bd(v: u64) -> BigDecimal {
    BigDecimal::from(v)
}

pub fn u128_bd(v: u128) -> BigDecimal {
    // `BigDecimal::from` isn't impl'd for u128; the decimal-string form is
    // unambiguous and parses without precision loss.
    BigDecimal::from_str(&v.to_string()).expect("u128 decimal is always valid")
}

pub fn bd_to_u128(v: &BigDecimal) -> anyhow::Result<u128> {
    v.to_u128()
        .ok_or_else(|| anyhow::anyhow!("value {v} doesn't fit in u128"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u128_round_trips_through_bigdecimal() {
        let big = u128::MAX;
        assert_eq!(bd_to_u128(&u128_bd(big)).unwrap(), big);
    }
}
