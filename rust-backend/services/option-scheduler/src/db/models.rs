//! Diesel row types for the scheduler_rolls table.

use chrono::{DateTime, Utc};
use diesel::prelude::*;

use super::schema::{scheduler_rolls, scheduler_vaults};

/// State of a scheduler roll row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollState {
    Pending,
    Submitted,
    Confirmed,
    NeedsReconciliation,
}

impl RollState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Submitted => "submitted",
            Self::Confirmed => "confirmed",
            Self::NeedsReconciliation => "needs_reconciliation",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "submitted" => Some(Self::Submitted),
            "confirmed" => Some(Self::Confirmed),
            "needs_reconciliation" => Some(Self::NeedsReconciliation),
            _ => None,
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Pending | Self::Submitted | Self::Confirmed | Self::NeedsReconciliation
        )
    }
}

#[derive(Queryable, Identifiable, Debug, Clone)]
#[diesel(table_name = scheduler_rolls)]
#[diesel(primary_key(id))]
pub struct SchedulerRollRow {
    pub id: i64,
    pub underlying_symbol: String,
    pub settlement_symbol: String,
    pub expiry_ms: i64,
    pub expiry_interval_ms: i64,
    pub state: String,
    pub tx_digest: Option<String>,
    pub bucket_ids: Option<serde_json::Value>,
    pub confirmed_bucket_ids: Option<serde_json::Value>,
    pub retry_count: i32,
    pub last_error: Option<String>,
    pub submit_anchor_seq: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl SchedulerRollRow {
    pub fn state_enum(&self) -> Option<RollState> {
        RollState::parse(&self.state)
    }
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = scheduler_rolls)]
pub struct NewSchedulerRoll<'a> {
    pub underlying_symbol: &'a str,
    pub settlement_symbol: &'a str,
    pub expiry_ms: i64,
    pub expiry_interval_ms: i64,
    pub state: &'a str,
    pub submit_anchor_seq: Option<i64>,
}

/// State of a scheduler vault row. See `migrations/0002_scheduler_vaults`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultState {
    Pending,
    CoinPublished,
    Confirmed,
    Failed,
}

impl VaultState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::CoinPublished => "coin_published",
            Self::Confirmed => "confirmed",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "coin_published" => Some(Self::CoinPublished),
            "confirmed" => Some(Self::Confirmed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    /// Rows that hold the pair's active-slot index (block a duplicate create).
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Pending | Self::CoinPublished | Self::Confirmed)
    }
}

#[derive(Queryable, Identifiable, Debug, Clone)]
#[diesel(table_name = scheduler_vaults)]
#[diesel(primary_key(id))]
pub struct SchedulerVaultRow {
    pub id: i64,
    pub underlying_symbol: String,
    pub settlement_symbol: String,
    pub round_ms: i64,
    pub state: String,
    pub share_coin_package: Option<String>,
    pub share_coin_type: Option<String>,
    pub share_cap_id: Option<String>,
    pub vault_id: Option<String>,
    pub publish_digest: Option<String>,
    pub create_digest: Option<String>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl SchedulerVaultRow {
    pub fn state_enum(&self) -> Option<VaultState> {
        VaultState::parse(&self.state)
    }
}

#[derive(Insertable, Debug, Clone)]
#[diesel(table_name = scheduler_vaults)]
pub struct NewSchedulerVault<'a> {
    pub underlying_symbol: &'a str,
    pub settlement_symbol: &'a str,
    pub round_ms: i64,
    pub state: &'a str,
}
