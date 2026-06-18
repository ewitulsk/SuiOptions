//! Diesel row types.

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use diesel::prelude::*;

use super::schema::{pool_mids, pool_trades, vault_predicted_apy, vault_realized_apy, watch_cursor};

#[derive(Insertable, Queryable, Debug, Clone)]
#[diesel(table_name = pool_trades)]
pub struct TradeRow {
    pub time: DateTime<Utc>,
    pub pool_id: String,
    pub bucket_id: String,
    pub price: f64,
    pub price_raw: BigDecimal,
    pub base_qty: BigDecimal,
    pub quote_qty: BigDecimal,
    pub base_decimals: i16,
    pub taker_is_bid: bool,
    pub tx_digest: String,
    pub event_index: i64,
}

#[derive(Insertable, Queryable, Debug, Clone)]
#[diesel(table_name = pool_mids)]
pub struct MidRow {
    pub time: DateTime<Utc>,
    pub pool_id: String,
    pub bucket_id: String,
    pub best_bid: f64,
    pub best_ask: f64,
    pub mid: f64,
}

#[derive(Insertable, Queryable, AsChangeset, Debug, Clone)]
#[diesel(table_name = watch_cursor)]
pub struct CursorRow {
    pub id: i16,
    pub cursor_tx: String,
    pub cursor_ev: i64,
    pub updated_at: DateTime<Utc>,
}

/// One predicted-APY point appended by the apy sampler (see `apy_sampler`).
#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = vault_predicted_apy)]
pub struct PredictedApyRow {
    pub time: DateTime<Utc>,
    pub vault_id: String,
    /// `current` (Tier 1) | `forecast` (Tier 2).
    pub kind: String,
    /// 0 = current round; 1..=K = rounds ahead.
    pub horizon: i32,
    pub t_ms: i64,
    pub apy: f64,
    pub confidence: f64,
}

/// One realized-APY point (annualized pps growth at a round's finalize time),
/// mirrored from the indexer's `vaultApy` series. Idempotent on (vault, round).
#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = vault_realized_apy)]
pub struct RealizedApyRow {
    pub time: DateTime<Utc>,
    pub vault_id: String,
    pub round: i64,
    pub apy: f64,
}
