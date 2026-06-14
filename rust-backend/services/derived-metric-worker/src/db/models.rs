use chrono::{DateTime, Utc};
use diesel::prelude::*;

use super::schema::vault_predictions;

/// A predicted-APY point to upsert.
#[derive(Debug, Clone, Insertable)]
#[diesel(table_name = vault_predictions)]
pub struct NewPrediction {
    pub vault_id: String,
    /// `current` (Tier 1) | `forecast` (Tier 2).
    pub kind: String,
    /// 0 = current round; 1..=K = rounds ahead.
    pub horizon: i32,
    pub t_ms: i64,
    pub apy: f64,
    pub confidence: f64,
    pub computed_at: DateTime<Utc>,
}

/// A stored prediction row, read back for the read API.
#[derive(Debug, Clone, Queryable)]
pub struct PredictionRow {
    pub vault_id: String,
    pub kind: String,
    pub horizon: i32,
    pub t_ms: i64,
    pub apy: f64,
    pub confidence: f64,
    pub computed_at: DateTime<Utc>,
}
