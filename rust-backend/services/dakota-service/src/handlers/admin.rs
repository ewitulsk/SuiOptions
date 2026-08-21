//! Admin-only operations: sandbox simulation, webhook registration, resync.

use std::sync::Arc;

use auth_client::VerifiedClaims;
use axum::extract::State;
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use uuid::Uuid;

use super::{bad_request, internal, ApiError};
use crate::authz::{authorize_customer, Caller};
use crate::dakota::types::*;
use crate::state::AppState;
use crate::webhook;

// ------------------------------------------------------------------ sandbox

#[derive(Deserialize)]
pub struct SimulateOnboardingBody {
    pub customer_id: String,
    /// Defaults to `kyb_approve`, which is the transition that actually moves
    /// the state machine — including for individuals, where `kyc_approve` is a
    /// no-op from `not_started`.
    #[serde(default)]
    pub r#type: Option<String>,
}

/// `POST /admin/sandbox/onboarding` — drive a customer to approved.
///
/// Takes a **customer** id and looks up the application id itself: Dakota's
/// `applicant_id` field wants the application, and passing the customer id
/// (which the docs' example implies) silently does nothing.
pub async fn simulate_onboarding(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
    Json(body): Json<SimulateOnboardingBody>,
) -> Result<Json<SimulationResp>, ApiError> {
    let caller = Caller::from_claims(&claims)?;
    caller.require_admin()?;
    let customer = authorize_customer(&state, &caller, &body.customer_id)?;

    let application_id = customer
        .application_id
        .clone()
        .ok_or_else(|| bad_request("customer has no onboarding application"))?;

    let resp: SimulationResp = state
        .dakota
        .post(
            "POST /sandbox/simulate/onboarding",
            "/sandbox/simulate/onboarding",
            &SimulateOnboardingReq {
                r#type: body.r#type.unwrap_or_else(|| "kyb_approve".into()),
                applicant_id: application_id,
                simulation_id: Uuid::new_v4().to_string(),
            },
        )
        .await
        .map_err(|e| (e.client_status(), e.to_string()))?;

    // Refresh our copy of the status straight away.
    //
    // `POST /accounts` gates on the LOCAL `kyb_status`, and the webhook that
    // would otherwise update it is asynchronous. Without this an operator
    // clicks Approve, sees "approved", and the very next ramp is still refused
    // as pending — which is exactly what the smoke test caught. Re-reading is
    // one call and makes the button mean what it says.
    if let Ok(fresh) = state
        .dakota
        .get::<CustomerStatus>(
            "GET /customers/{id}",
            &format!("/customers/{}", body.customer_id),
        )
        .await
    {
        let _ = state.repo.upsert_customer(&crate::db::models::UpsertCustomer {
            dakota_customer_id: customer.dakota_customer_id.clone(),
            customer_type: customer.customer_type.clone(),
            is_sub_client: customer.is_sub_client,
            sub_client_id: customer.sub_client_id.clone(),
            external_ref: customer.external_ref.clone(),
            application_id: fresh.application_id.clone(),
            kyb_status: fresh.kyb_status.clone(),
            kyc_status: fresh.kyc_status.clone(),
            application_status: fresh.application_status.clone(),
        });
    }

    info!(
        customer_id = %body.customer_id,
        previous = resp.previous_state.as_deref().unwrap_or("-"),
        new = resp.new_state.as_deref().unwrap_or("-"),
        "onboarding simulated"
    );
    Ok(Json(resp))
}

#[derive(Deserialize)]
pub struct SimulateInboundBody {
    /// `ach_inbound` | `fedwire_inbound` | `fednow_inbound` | `crypto_inbound`.
    pub r#type: String,
    /// Decimal string, e.g. "2.00".
    pub amount: String,
    #[serde(default = "usd")]
    pub currency: String,
    /// Required for fiat inbound types.
    #[serde(default)]
    pub account_id: Option<String>,
    /// Required for `crypto_inbound`.
    #[serde(default)]
    pub wallet_address: Option<String>,
    #[serde(default)]
    pub scenario: Option<String>,
}

fn usd() -> String {
    "USD".to_string()
}

/// `POST /admin/sandbox/inbound` — fund a ramp without moving real money.
pub async fn simulate_inbound(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
    Json(body): Json<SimulateInboundBody>,
) -> Result<Json<SimulationResp>, ApiError> {
    Caller::from_claims(&claims)?.require_admin()?;

    // Check the cap locally: Dakota's rejection is clear, but this saves a
    // round-trip and states the limit in the same units the caller typed.
    let minor = parse_minor(&body.amount)
        .ok_or_else(|| bad_request(format!("amount {:?} is not a decimal string", body.amount)))?;
    if minor > state.cfg.dakota.max_amount_minor {
        return Err(bad_request(format!(
            "amount {} exceeds the configured cap of {}.{:02}",
            body.amount,
            state.cfg.dakota.max_amount_minor / 100,
            state.cfg.dakota.max_amount_minor % 100
        )));
    }

    state
        .dakota
        .post(
            "POST /sandbox/simulate/inbound",
            "/sandbox/simulate/inbound",
            &SimulateInboundReq {
                simulation_id: Uuid::new_v4().to_string(),
                r#type: body.r#type,
                amount: body.amount,
                currency: body.currency,
                account_id: body.account_id,
                wallet_address: body.wallet_address,
                scenario: body.scenario,
            },
        )
        .await
        .map(Json)
        .map_err(|e| (e.client_status(), e.to_string()))
}

/// Decimal string -> minor units. Mirrors `webhook::minor_units`; kept separate
/// because this one rejects rather than silently returning `None` downstream.
fn parse_minor(s: &str) -> Option<i64> {
    let s = s.trim();
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    if whole.is_empty() || !whole.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if !frac.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let units: i64 = whole.parse().ok()?;
    let cents: i64 = frac
        .chars()
        .chain(std::iter::repeat('0'))
        .take(2)
        .collect::<String>()
        .parse()
        .ok()?;
    units.checked_mul(100)?.checked_add(cents)
}

// ----------------------------------------------------------------- webhooks

#[derive(Serialize)]
pub struct WebhookRegistration {
    pub url: String,
    pub result: serde_json::Value,
}

/// `POST /admin/webhooks/register` — point Dakota at this deployment.
///
/// Deliberately a manual action rather than something done at boot: registering
/// on every restart churns targets, and the URL depends on how the environment
/// is proxied rather than on anything the process can discover.
pub async fn register_webhook(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
) -> Result<Json<WebhookRegistration>, ApiError> {
    Caller::from_claims(&claims)?.require_admin()?;

    let url = state
        .cfg
        .dakota
        .webhook_url
        .clone()
        .ok_or_else(|| bad_request("dakota.webhook_url is not configured"))?;

    let result: serde_json::Value = state
        .dakota
        .post(
            "POST /webhooks/targets",
            "/webhooks/targets",
            &CreateWebhookTargetReq { url: url.clone(), event_types: None },
        )
        .await
        .map_err(|e| (e.client_status(), e.to_string()))?;

    info!(%url, "registered dakota webhook target");
    Ok(Json(WebhookRegistration { url, result }))
}

/// `GET /admin/webhooks` — current targets, so an operator can see duplicates.
pub async fn list_webhooks(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Caller::from_claims(&claims)?.require_admin()?;
    state
        .dakota
        .get("GET /webhooks/targets", "/webhooks/targets")
        .await
        .map(Json)
        .map_err(|e| (e.client_status(), e.to_string()))
}

// ------------------------------------------------------------------- resync

#[derive(Serialize)]
pub struct ResyncResult {
    pub scanned: usize,
    pub inserted: usize,
    /// Dakota had more events than one page held. Surfaced so a partial
    /// backfill is never mistaken for a complete one.
    pub truncated: bool,
}

/// `POST /admin/resync` — rebuild the ledger from Dakota's event log.
///
/// Webhooks are the primary path, but they can be missed: a target registered
/// late, a deployment down past the 48-hour retry window, a delivery that
/// failed to parse. This replays `GET /events` through the same extractor, and
/// because `record_event` is keyed on the event id, replaying is safe.
pub async fn resync(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
) -> Result<Json<ResyncResult>, ApiError> {
    Caller::from_claims(&claims)?.require_admin()?;

    // 100 is Dakota's hard maximum — asking for more is a 400, not a silent
    // clamp ("Query parameter 'limit' has invalid value: number must be at
    // most 100").
    let page: serde_json::Value = state
        .dakota
        .get("GET /events", "/events?limit=100")
        .await
        .map_err(|e| (e.client_status(), e.to_string()))?;

    let rows = page
        .get("data")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();

    // One page only. Say so rather than letting a partial backfill look
    // complete — a caller who reads "scanned 100" and stops has a gap they do
    // not know about.
    let truncated = page
        .pointer("/meta/has_more_after")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if truncated {
        warn!(
            scanned = rows.len(),
            "dakota has more events than one page; run resync again to continue"
        );
    }

    let mut inserted = 0usize;
    for row in &rows {
        // Dakota's event objects carry their own id; without one there is no
        // idempotency key and replaying would duplicate the row.
        let Some(event_id) = row
            .get("id")
            .or_else(|| row.get("event_id"))
            .and_then(|v| v.as_str())
        else {
            continue;
        };
        let mut event = webhook::extract_for_resync(event_id, row);
        if event.dakota_customer_id.is_none() {
            if let Some(acct) = webhook::account_ref(row) {
                event.dakota_customer_id = state.repo.account_owner(&acct).ok().flatten();
            }
        }
        if state.repo.record_event(&event).map_err(internal)? {
            inserted += 1;
        }
    }

    info!(scanned = rows.len(), inserted, truncated, "resynced ledger from dakota events");
    Ok(Json(ResyncResult { scanned: rows.len(), inserted, truncated }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minor_handles_the_shapes_operators_type() {
        assert_eq!(parse_minor("2"), Some(200));
        assert_eq!(parse_minor("2.00"), Some(200));
        assert_eq!(parse_minor("1.5"), Some(150));
        assert_eq!(parse_minor("0.05"), Some(5));
        assert_eq!(parse_minor(" 2.00 "), Some(200));
    }

    #[test]
    fn parse_minor_rejects_nonsense() {
        assert_eq!(parse_minor("abc"), None);
        assert_eq!(parse_minor(""), None);
        assert_eq!(parse_minor("-1.00"), None, "negatives are not deposits");
        assert_eq!(parse_minor("1.2.3"), None);
        assert_eq!(parse_minor("$2.00"), None);
    }

    #[test]
    fn sandbox_cap_boundary() {
        // $2.00 is the documented sandbox ceiling: allowed, and a cent more is
        // not.
        assert!(parse_minor("2.00").unwrap() <= 200);
        assert!(parse_minor("2.01").unwrap() > 200);
    }
}
