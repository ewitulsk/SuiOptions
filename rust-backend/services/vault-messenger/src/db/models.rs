//! Diesel row types + status/direction constants.

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use diesel::prelude::*;

use super::schema::vault_messages;

/// Message status state machine: `pending → submitted → confirmed`,
/// terminal `failed`. Hub-side deliveries confirm synchronously (the PTB
/// submit waits for finality), so they jump `pending → confirmed`.
pub mod status {
    pub const PENDING: &str = "pending";
    pub const SUBMITTED: &str = "submitted";
    pub const CONFIRMED: &str = "confirmed";
    pub const FAILED: &str = "failed";
}

pub mod direction {
    pub const SPOKE_TO_HUB: &str = "spoke_to_hub";
    pub const HUB_TO_SPOKE: &str = "hub_to_spoke";
}

#[derive(Queryable, Identifiable, Debug, Clone)]
#[diesel(table_name = vault_messages)]
pub struct MessageRow {
    pub id: i64,
    pub direction: String,
    pub spoke_id: i64,
    pub seq: i64,
    pub msg_type: i16,
    pub message_hex: String,
    pub status: String,
    pub attempts: i32,
    pub tx_hash: Option<String>,
    pub error: Option<String>,
    pub observed_tx: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = vault_messages)]
pub struct NewMessage {
    pub direction: String,
    pub spoke_id: i64,
    pub seq: i64,
    pub msg_type: i16,
    pub message_hex: String,
    pub status: String,
    pub observed_tx: Option<String>,
}

#[derive(Queryable, Debug, Clone)]
pub struct PayableRow {
    pub spoke_id: i64,
    pub request_seq: i64,
    pub pay_units: BigDecimal,
    pub created_at: DateTime<Utc>,
    pub settled_at: Option<DateTime<Utc>>,
}

#[derive(Queryable, Debug, Clone)]
pub struct LaneStatsRow {
    pub spoke_id: i64,
    pub fee_pot: BigDecimal,
    pub last_state_sync_ms: i64,
    pub updated_at: DateTime<Utc>,
}
