//! Diesel row types.

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use diesel::prelude::*;

use super::schema::{pool_trades, watch_cursor};

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

#[derive(Insertable, Queryable, AsChangeset, Debug, Clone)]
#[diesel(table_name = watch_cursor)]
pub struct CursorRow {
    pub id: i16,
    pub cursor_tx: String,
    pub cursor_ev: i64,
    pub updated_at: DateTime<Utc>,
}
