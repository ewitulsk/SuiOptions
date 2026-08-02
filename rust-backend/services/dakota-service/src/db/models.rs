//! Row structs for the read model. Nothing here holds PII — see the policy
//! note at the top of `migrations/000001_init/up.sql`.

use chrono::{DateTime, Utc};
use diesel::prelude::*;
use serde::{Deserialize, Serialize};

use super::schema::{accounts, assets, customers, fee_schedule, ledger_events, wallets, webhook_errors};

// -------------------------------------------------------------------- assets

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = assets)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Asset {
    pub id: i32,
    pub symbol: String,
    pub network_id: String,
    pub onramp_enabled: bool,
    pub offramp_enabled: bool,
    pub swap_enabled: bool,
    pub sort_order: i32,
    #[serde(skip)]
    pub created_at: DateTime<Utc>,
    #[serde(skip)]
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Insertable, AsChangeset, Deserialize)]
#[diesel(table_name = assets)]
pub struct UpsertAsset {
    pub symbol: String,
    pub network_id: String,
    #[serde(default)]
    pub onramp_enabled: bool,
    #[serde(default)]
    pub offramp_enabled: bool,
    #[serde(default)]
    pub swap_enabled: bool,
    #[serde(default)]
    pub sort_order: i32,
}

// -------------------------------------------------------------- fee schedule

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = fee_schedule)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct FeeSchedule {
    pub id: i32,
    /// `manual` (admin-entered) or `dakota` (fetched). Surfaced to the UI so a
    /// hand-typed rate is never displayed as if Dakota had confirmed it.
    pub source: String,
    pub transfer_fee_bps: Option<i32>,
    pub ach_fee_cents: Option<i32>,
    pub wire_fee_cents: Option<i32>,
    pub sepa_fee_cents: Option<i32>,
    pub swift_fee_cents: Option<i32>,
    pub kyc_fee_cents: Option<i32>,
    pub kyb_fee_cents: Option<i32>,
    pub effective_from: DateTime<Utc>,
    pub fetched_at: DateTime<Utc>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Insertable, Deserialize)]
#[diesel(table_name = fee_schedule)]
pub struct NewFeeSchedule {
    #[serde(default = "manual_source")]
    pub source: String,
    pub transfer_fee_bps: Option<i32>,
    pub ach_fee_cents: Option<i32>,
    pub wire_fee_cents: Option<i32>,
    pub sepa_fee_cents: Option<i32>,
    pub swift_fee_cents: Option<i32>,
    pub kyc_fee_cents: Option<i32>,
    pub kyb_fee_cents: Option<i32>,
    pub note: Option<String>,
}

fn manual_source() -> String {
    "manual".to_string()
}

// ----------------------------------------------------------------- customers

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = customers)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Customer {
    pub dakota_customer_id: String,
    pub customer_type: String,
    pub is_sub_client: bool,
    pub sub_client_id: Option<String>,
    pub external_ref: Option<String>,
    pub application_id: Option<String>,
    pub kyb_status: Option<String>,
    pub kyc_status: Option<String>,
    pub application_status: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Insertable, AsChangeset)]
#[diesel(table_name = customers)]
pub struct UpsertCustomer {
    pub dakota_customer_id: String,
    pub customer_type: String,
    pub is_sub_client: bool,
    pub sub_client_id: Option<String>,
    pub external_ref: Option<String>,
    pub application_id: Option<String>,
    pub kyb_status: Option<String>,
    pub kyc_status: Option<String>,
    pub application_status: Option<String>,
}

// ------------------------------------------------------------------ accounts

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = accounts)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Account {
    pub dakota_account_id: String,
    pub dakota_customer_id: String,
    pub account_type: String,
    pub source_asset: Option<String>,
    pub source_network_id: Option<String>,
    pub destination_asset: Option<String>,
    pub destination_network_id: Option<String>,
    pub rail: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Insertable)]
#[diesel(table_name = accounts)]
pub struct NewAccount {
    pub dakota_account_id: String,
    pub dakota_customer_id: String,
    pub account_type: String,
    pub source_asset: Option<String>,
    pub source_network_id: Option<String>,
    pub destination_asset: Option<String>,
    pub destination_network_id: Option<String>,
    pub rail: Option<String>,
}

// ------------------------------------------------------------------- ledger

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = ledger_events)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct LedgerEvent {
    pub event_id: String,
    pub event_type: String,
    pub resource_type: Option<String>,
    pub resource_id: Option<String>,
    pub dakota_customer_id: Option<String>,
    pub direction: Option<String>,
    pub amount_minor: Option<i64>,
    pub asset: Option<String>,
    pub exchange_rate: Option<String>,
    pub fee_minor: Option<i64>,
    pub status: Option<String>,
    pub occurred_at: Option<DateTime<Utc>>,
    pub received_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Insertable)]
#[diesel(table_name = ledger_events)]
pub struct NewLedgerEvent {
    pub event_id: String,
    pub event_type: String,
    pub resource_type: Option<String>,
    pub resource_id: Option<String>,
    pub dakota_customer_id: Option<String>,
    pub direction: Option<String>,
    pub amount_minor: Option<i64>,
    pub asset: Option<String>,
    pub exchange_rate: Option<String>,
    pub fee_minor: Option<i64>,
    pub status: Option<String>,
    pub occurred_at: Option<DateTime<Utc>>,
}

// ------------------------------------------------------------------ wallets

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = wallets)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Wallet {
    pub dakota_wallet_id: String,
    pub address: Option<String>,
    pub family: String,
    pub signer_group_id: Option<String>,
    pub policy_id: Option<String>,
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Insertable)]
#[diesel(table_name = wallets)]
pub struct NewWallet {
    pub dakota_wallet_id: String,
    pub address: Option<String>,
    pub family: String,
    pub signer_group_id: Option<String>,
    pub policy_id: Option<String>,
    pub label: Option<String>,
}

// ----------------------------------------------------------- webhook errors

#[derive(Debug, Clone, Insertable)]
#[diesel(table_name = webhook_errors)]
pub struct NewWebhookError {
    pub event_id: Option<String>,
    pub reason: String,
    /// Digest of the body, never the body. A delivery that failed to parse is
    /// exactly as likely to hold PII as one that succeeded.
    pub body_sha256: String,
}
