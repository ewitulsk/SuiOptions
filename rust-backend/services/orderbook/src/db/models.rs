//! Insertable / queryable row types for the exchange schema.

use super::schema::{exchange_fills, exchange_markets, exchange_orders};
use diesel::prelude::*;
use serde::Serialize;

#[derive(Insertable, AsChangeset)]
#[diesel(table_name = exchange_markets)]
pub struct NewMarket {
    pub registry_id: String,
    pub symbol: String,
    pub base: String,
    pub quote: String,
    pub tick_size: i64,
    pub min_size: i64,
    pub lot_size: i64,
    pub current_fee_bps: i64,
}

#[derive(Insertable)]
#[diesel(table_name = exchange_orders)]
pub struct NewOrder {
    pub digest: String,
    pub registry_id: String,
    pub maker: String,
    pub manager_id: String,
    pub maker_token: String,
    pub side: String,
    pub price_ticks: i64,
    pub salt: i64,
    pub expiry_ms: i64,
    pub taker_amount: i64,
    pub maker_amount: i64,
    pub order_json: serde_json::Value,
    pub order_bytes: Vec<u8>,
}

/// The columns the service reads back for an order.
#[derive(Queryable)]
pub struct OrderRow {
    pub digest: String,
    pub side: String,
    pub price_ticks: i64,
    pub filled_taker: i64,
    pub status: String,
    pub order_json: serde_json::Value,
}

#[derive(Insertable)]
#[diesel(table_name = exchange_fills)]
pub struct NewFill {
    pub tx_digest: String,
    pub event_seq: i64,
    pub digest: String,
    pub registry_id: String,
    pub maker: String,
    pub taker: String,
    pub base_amount: i64,
    pub quote_amount: i64,
    pub maker_fee: i64,
    pub taker_fee: i64,
    pub maker_sold_base: bool,
    pub filled_total: i64,
    pub timestamp_ms: i64,
}

/// A chain-confirmed fill as served by the API.
#[derive(Queryable, Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FillRow {
    pub tx_digest: String,
    pub event_seq: i64,
    pub digest: String,
    pub registry_id: String,
    pub maker: String,
    pub taker: String,
    pub base_amount: i64,
    pub quote_amount: i64,
    pub maker_fee: i64,
    pub taker_fee: i64,
    pub maker_sold_base: bool,
    pub filled_total: i64,
    pub timestamp_ms: i64,
}
