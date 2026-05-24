use std::sync::Arc;

use axum::{extract::State, Json};
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
pub struct BucketDto {
    pub bucket_id: String,
    pub asset_type: String,
    pub settlement_type: String,
    /// Stringified to avoid JSON precision loss on u64.
    pub strike: String,
    pub expiry_ms: String,
    pub total_written: String,
    pub exercise_cursor: String,
}

#[derive(Serialize)]
pub struct BucketsResponse {
    pub buckets: Vec<BucketDto>,
}

pub async fn list_buckets(State(state): State<Arc<AppState>>) -> Json<BucketsResponse> {
    let buckets = state
        .active_buckets()
        .into_iter()
        .map(|(id, b)| BucketDto {
            bucket_id: id.to_hex(),
            asset_type: b.asset_type.as_str().to_string(),
            settlement_type: b.settlement_type.as_str().to_string(),
            strike: b.strike.to_string(),
            expiry_ms: b.expiry_ms.to_string(),
            total_written: b.total_written.to_string(),
            exercise_cursor: b.exercise_cursor.to_string(),
        })
        .collect();
    Json(BucketsResponse { buckets })
}
