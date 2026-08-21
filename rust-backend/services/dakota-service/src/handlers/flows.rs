//! Activity and amount-flow tracking.
//!
//! Everything here reads the local `ledger_events` table rather than Dakota,
//! for three reasons: it is already scoped to our hierarchy, it aggregates
//! without N round-trips, and it contains no PII — Dakota's own
//! `GET /events` payload carries sender names and bank account numbers.

use std::sync::Arc;

use auth_client::VerifiedClaims;
use axum::extract::{Path, Query, State};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};

use super::{internal, ApiError};
use crate::authz::{authorize_customer, Caller};
use crate::db::models::LedgerEvent;
use crate::db::repo::CustomerFlow;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct FeedQuery {
    #[serde(default = "default_limit")]
    pub limit: i64,
}

fn default_limit() -> i64 {
    100
}

#[derive(Serialize)]
pub struct FlowsResp {
    /// Per-customer, per-asset totals for whatever the caller may see.
    pub by_customer: Vec<CustomerFlow>,
    /// Platform- or roster-wide totals per asset.
    pub totals: Vec<AssetTotal>,
}

#[derive(Serialize, Default)]
pub struct AssetTotal {
    pub asset: String,
    pub inbound_minor: i64,
    pub outbound_minor: i64,
    pub events: i64,
}

/// `GET /flows` — the tracking view.
///
/// Admin sees the platform; a business sees its own roster; an individual sees
/// only itself.
pub async fn get_flows(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
) -> Result<Json<FlowsResp>, ApiError> {
    let caller = Caller::from_claims(&claims)?;

    let by_customer = match &caller {
        Caller::Individual { customer_id } => state
            .repo
            .customer_flows(None)
            .map_err(internal)?
            .into_iter()
            .filter(|f| &f.dakota_customer_id == customer_id)
            .collect(),
        _ => state
            .repo
            .customer_flows(caller.sub_client_filter())
            .map_err(internal)?,
    };

    Ok(Json(FlowsResp {
        totals: totals_by_asset(&by_customer),
        by_customer,
    }))
}

/// `GET /flows/:customer_id` — one customer's timeline.
pub async fn customer_feed(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
    Path(customer_id): Path<String>,
    Query(q): Query<FeedQuery>,
) -> Result<Json<Vec<LedgerEvent>>, ApiError> {
    let caller = Caller::from_claims(&claims)?;
    authorize_customer(&state, &caller, &customer_id)?;
    state
        .repo
        .list_events(Some(&customer_id), q.limit)
        .map(Json)
        .map_err(internal)
}

/// `GET /flows/feed` — recent activity across everything the caller may see.
pub async fn feed(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
    Query(q): Query<FeedQuery>,
) -> Result<Json<Vec<LedgerEvent>>, ApiError> {
    let caller = Caller::from_claims(&claims)?;
    let rows = match &caller {
        Caller::Admin => state.repo.list_events(None, q.limit).map_err(internal)?,
        Caller::Individual { customer_id } => state
            .repo
            .list_events(Some(customer_id), q.limit)
            .map_err(internal)?,
        Caller::Business { .. } => {
            // Filter the roster in memory. Fine at sandbox volumes; if this
            // ever gets slow the fix is an index-backed IN query, not a
            // per-customer fan-out.
            let roster: std::collections::HashSet<String> = state
                .repo
                .list_customers(caller.sub_client_filter())
                .map_err(internal)?
                .into_iter()
                .map(|c| c.dakota_customer_id)
                .collect();
            state
                .repo
                .list_events(None, q.limit)
                .map_err(internal)?
                .into_iter()
                .filter(|e| {
                    e.dakota_customer_id
                        .as_deref()
                        .is_some_and(|id| roster.contains(id))
                })
                .collect()
        }
    };
    Ok(Json(rows))
}

/// Roll per-customer rows up per asset.
///
/// `NULL` asset rows come from the LEFT JOIN — a customer with no activity —
/// and are skipped so a brand-new customer does not invent an empty asset.
fn totals_by_asset(rows: &[CustomerFlow]) -> Vec<AssetTotal> {
    let mut acc: std::collections::BTreeMap<String, AssetTotal> = Default::default();
    for r in rows {
        let Some(asset) = r.asset.clone() else { continue };
        let e = acc.entry(asset.clone()).or_insert_with(|| AssetTotal {
            asset,
            ..Default::default()
        });
        e.inbound_minor += r.inbound_minor.unwrap_or(0);
        e.outbound_minor += r.outbound_minor.unwrap_or(0);
        e.events += r.events;
    }
    acc.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cus: &str, asset: Option<&str>, inb: Option<i64>, outb: Option<i64>, n: i64) -> CustomerFlow {
        CustomerFlow {
            dakota_customer_id: cus.into(),
            customer_type: "individual".into(),
            sub_client_id: None,
            asset: asset.map(|s| s.into()),
            events: n,
            inbound_minor: inb,
            outbound_minor: outb,
        }
    }

    #[test]
    fn totals_sum_per_asset_across_customers() {
        let t = totals_by_asset(&[
            row("c1", Some("USDC"), Some(200), None, 1),
            row("c2", Some("USDC"), Some(150), Some(50), 2),
            row("c3", Some("RD"), None, Some(75), 1),
        ]);
        assert_eq!(t.len(), 2);
        let usdc = t.iter().find(|a| a.asset == "USDC").unwrap();
        assert_eq!(usdc.inbound_minor, 350);
        assert_eq!(usdc.outbound_minor, 50);
        assert_eq!(usdc.events, 3);
        let rd = t.iter().find(|a| a.asset == "RD").unwrap();
        assert_eq!(rd.outbound_minor, 75);
        assert_eq!(rd.inbound_minor, 0);
    }

    #[test]
    fn customers_with_no_activity_do_not_invent_an_asset() {
        // The LEFT JOIN emits a NULL-asset row for a customer with no events.
        let t = totals_by_asset(&[row("c1", None, None, None, 0)]);
        assert!(t.is_empty());
    }

    #[test]
    fn empty_input_is_empty_output() {
        assert!(totals_by_asset(&[]).is_empty());
    }
}
