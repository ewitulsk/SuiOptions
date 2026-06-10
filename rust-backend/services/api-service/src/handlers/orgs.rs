//! `GET /orgs` — the verified-orgs allowlist, for the frontend.
//!
//! Served from the in-process [`token_info_client::VerifiedOrgsWatcher`]
//! (polled from token-info) so the frontend has a single API base for all
//! read traffic. Only enabled (verified) orgs appear — this is the same set
//! `/buckets` filters by.

use std::sync::Arc;

use axum::{extract::State, Json};
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
pub struct OrgDto {
    pub org_id: String,
    pub name: String,
}

#[derive(Serialize)]
pub struct OrgsResponse {
    pub orgs: Vec<OrgDto>,
}

pub async fn list_orgs(State(state): State<Arc<AppState>>) -> Json<OrgsResponse> {
    let mut orgs: Vec<OrgDto> = state
        .verified_orgs
        .all()
        .into_iter()
        .map(|o| OrgDto {
            org_id: o.org_id,
            name: o.name,
        })
        .collect();
    orgs.sort_by(|a, b| a.name.cmp(&b.name));
    Json(OrgsResponse { orgs })
}
