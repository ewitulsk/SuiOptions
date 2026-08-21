//! Ramps: recipients, destinations and onramp / offramp / swap accounts.
//!
//! All three ramps are one Dakota call (`POST /accounts`) with a different
//! `account_type`, but each has a prerequisite the API will not tell you about
//! until it rejects you:
//!
//! - the customer must be **KYB-approved** (`kyb_status == "active"`), for
//!   individuals as much as businesses;
//! - an **onramp** must send `capabilities`, which is undocumented as required;
//! - an **offramp** needs a fiat destination, which needs a recipient *address*.
//!
//! We check what we can locally so the operator gets a sentence they can act
//! on instead of a 400 from three systems away.

use std::sync::Arc;

use auth_client::VerifiedClaims;
use axum::extract::{Path, Query, State};
use axum::{Extension, Json};
use serde::Deserialize;
use tracing::info;

use super::{bad_request, internal, ApiError};
use crate::authz::{authorize_customer, Caller};
use crate::dakota::types::*;
use crate::db::models::{Account, NewAccount};
use crate::state::AppState;

// ---------------------------------------------------- recipients + destinations

#[derive(Deserialize)]
pub struct CreateRecipientBody {
    pub name: String,
    /// Required before any fiat destination can be attached, so an offramp
    /// needs it. Passed through to Dakota verbatim and never stored.
    #[serde(default)]
    pub address: Option<serde_json::Value>,
}

/// `POST /customers/:id/recipients`
pub async fn create_recipient(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
    Path(customer_id): Path<String>,
    Json(body): Json<CreateRecipientBody>,
) -> Result<Json<CreatedId>, ApiError> {
    let caller = Caller::from_claims(&claims)?;
    authorize_customer(&state, &caller, &customer_id)?;

    state
        .dakota
        .post(
            "POST /customers/{id}/recipients",
            &format!("/customers/{customer_id}/recipients"),
            &CreateRecipientReq { name: body.name, address: body.address },
        )
        .await
        .map(Json)
        .map_err(|e| (e.client_status(), e.to_string()))
}

#[derive(Deserialize)]
pub struct CreateDestinationBody {
    pub customer_id: String,
    /// `crypto` | `fiat_us` | `fiat_iban` — Dakota discriminates on this.
    pub destination_type: String,
    #[serde(flatten)]
    pub rest: serde_json::Value,
}

/// `POST /recipients/:id/destinations`
///
/// The body is relayed as-is: fiat destinations carry account and routing
/// numbers, and binding them to a struct here would be the first step toward
/// accidentally logging or storing them.
pub async fn create_destination(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
    Path(recipient_id): Path<String>,
    Json(body): Json<CreateDestinationBody>,
) -> Result<Json<CreatedId>, ApiError> {
    let caller = Caller::from_claims(&claims)?;
    authorize_customer(&state, &caller, &body.customer_id)?;

    let mut payload = body.rest;
    payload["destination_type"] = serde_json::json!(body.destination_type);
    if let Some(network) = payload.get("network_id").and_then(|v| v.as_str()) {
        if !state.cfg.network_allowed(network) {
            return Err(bad_request(format!(
                "network {network} is not permitted in this environment"
            )));
        }
    }

    state
        .dakota
        .post(
            "POST /recipients/{id}/destinations",
            &format!("/recipients/{recipient_id}/destinations"),
            &payload,
        )
        .await
        .map(Json)
        .map_err(|e| (e.client_status(), e.to_string()))
}

// ------------------------------------------------------------------ accounts

#[derive(Deserialize)]
pub struct CreateAccountBody {
    pub customer_id: String,
    /// `onramp` | `offramp` | `swap`.
    pub account_type: String,
    #[serde(default)]
    pub crypto_destination_id: Option<String>,
    #[serde(default)]
    pub fiat_destination_id: Option<String>,
    #[serde(default)]
    pub source_asset: Option<String>,
    #[serde(default)]
    pub destination_asset: Option<String>,
    #[serde(default)]
    pub source_network_id: Option<String>,
    #[serde(default)]
    pub destination_network_id: Option<String>,
    /// Defaults to ACH + Fedwire for onramps, which Dakota requires.
    #[serde(default)]
    pub capabilities: Option<Vec<String>>,
    #[serde(default)]
    pub developer_fee_bps: Option<i32>,
}

/// `POST /accounts` — open a ramp.
///
/// Returns Dakota's response verbatim, because that is where the deposit
/// details live: `bank_account` (routing + account number + holder name) for an
/// onramp, `source_crypto_address` for an offramp or swap. The bank block is
/// PII and goes straight to the browser without touching the database.
pub async fn create_account(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
    Json(body): Json<CreateAccountBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let caller = Caller::from_claims(&claims)?;
    let customer = authorize_customer(&state, &caller, &body.customer_id)?;

    // Fail here rather than let Dakota answer "Customer is not KYB-approved by
    // Dakota", which reads like a business problem when it is usually just an
    // un-run sandbox simulation.
    if customer.kyb_status.as_deref() != Some("active") {
        return Err(bad_request(format!(
            "customer is not approved to transact (kyb_status = {}); \
             in sandbox, run the kyb_approve simulation first",
            customer.kyb_status.as_deref().unwrap_or("unknown")
        )));
    }

    match body.account_type.as_str() {
        "onramp" | "offramp" | "swap" => {}
        other => return Err(bad_request(format!("unknown account_type {other:?}"))),
    }

    // The catalog is the allow-list: an asset we have not enabled must not be
    // reachable just because someone typed it into a request body.
    let (asset, network) = match body.account_type.as_str() {
        // For an onramp the fiat side is the source; the stablecoin we deliver
        // is what the catalog governs.
        "onramp" => (body.destination_asset.as_deref(), body.destination_network_id.as_deref()),
        _ => (body.source_asset.as_deref(), body.source_network_id.as_deref()),
    };
    if let (Some(a), Some(n)) = (asset, network) {
        if !state.cfg.network_allowed(n) {
            return Err(bad_request(format!(
                "network {n} is not permitted in this environment"
            )));
        }
        if !state
            .repo
            .asset_allows(a, n, &body.account_type)
            .map_err(internal)?
        {
            return Err(bad_request(format!(
                "{a} on {n} is not enabled for {}",
                body.account_type
            )));
        }
    }

    let capabilities = match (body.account_type.as_str(), body.capabilities.clone()) {
        (_, Some(c)) => Some(c),
        // Undocumented as required; without it Dakota 400s "capabilities are
        // required".
        ("onramp", None) => Some(vec!["ach".to_string(), "fedwire".to_string()]),
        _ => None,
    };

    let req = CreateAccountReq {
        account_type: body.account_type.clone(),
        capabilities,
        crypto_destination_id: body.crypto_destination_id,
        fiat_destination_id: body.fiat_destination_id,
        destination_network_id: body.destination_network_id.clone(),
        source_network_id: body.source_network_id.clone(),
        source_asset: body.source_asset.clone(),
        destination_asset: body.destination_asset.clone(),
        developer_fee_bps: body.developer_fee_bps,
    };

    let raw: serde_json::Value = state
        .dakota
        .post("POST /accounts", "/accounts", &req)
        .await
        .map_err(|e| (e.client_status(), e.to_string()))?;

    // Index the non-identifying half.
    if let Ok(summary) = serde_json::from_value::<AccountSummary>(raw.clone()) {
        let _ = state.repo.insert_account(&NewAccount {
            dakota_account_id: summary.id.clone(),
            dakota_customer_id: body.customer_id.clone(),
            account_type: summary.account_type.clone(),
            source_asset: summary.source_asset.clone().or(body.source_asset),
            source_network_id: summary.source_network_id.clone().or(body.source_network_id),
            destination_asset: body.destination_asset,
            destination_network_id: body.destination_network_id,
            rail: summary.rail.clone(),
        });
        info!(
            account_id = %summary.id,
            customer_id = %body.customer_id,
            account_type = %summary.account_type,
            "ramp account opened"
        );
    }

    Ok(Json(raw))
}

#[derive(Deserialize)]
pub struct ListAccountsQuery {
    #[serde(default)]
    pub customer_id: Option<String>,
}

/// `GET /accounts`
pub async fn list_accounts(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
    Query(q): Query<ListAccountsQuery>,
) -> Result<Json<Vec<Account>>, ApiError> {
    let caller = Caller::from_claims(&claims)?;

    let rows = match (&caller, q.customer_id.as_deref()) {
        (_, Some(id)) => {
            authorize_customer(&state, &caller, id)?;
            state.repo.list_accounts(Some(id)).map_err(internal)?
        }
        (Caller::Admin, None) => state.repo.list_accounts(None).map_err(internal)?,
        (Caller::Individual { customer_id }, None) => state
            .repo
            .list_accounts(Some(customer_id))
            .map_err(internal)?,
        (Caller::Business { .. }, None) => {
            // Accounts have no sub_client column; fan out over the roster so a
            // business still gets one list rather than having to ask per
            // customer.
            let mut out = Vec::new();
            for c in state
                .repo
                .list_customers(caller.sub_client_filter())
                .map_err(internal)?
            {
                out.extend(
                    state
                        .repo
                        .list_accounts(Some(&c.dakota_customer_id))
                        .map_err(internal)?,
                );
            }
            out
        }
    };
    Ok(Json(rows))
}

/// `GET /accounts/:id` — live detail from Dakota, including deposit
/// instructions.
pub async fn get_account(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
    Path(account_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let caller = Caller::from_claims(&claims)?;
    let owner = state
        .repo
        .account_owner(&account_id)
        .map_err(internal)?
        .ok_or((axum::http::StatusCode::NOT_FOUND, "unknown account".to_string()))?;
    authorize_customer(&state, &caller, &owner)?;

    state
        .dakota
        .get("GET /accounts/{id}", &format!("/accounts/{account_id}"))
        .await
        .map(Json)
        .map_err(|e| (e.client_status(), e.to_string()))
}

// ------------------------------------------------------------- transactions

/// `GET /transactions` — auto-account transactions, relayed from Dakota.
///
/// Admin-only: Dakota's list is not scoped per customer, and the payload
/// carries sender bank details. Non-admins read their own history from the
/// ledger via `/flows` instead.
pub async fn list_transactions(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Caller::from_claims(&claims)?.require_admin()?;
    state
        .dakota
        .get("GET /auto-transactions", "/auto-transactions")
        .await
        .map(Json)
        .map_err(|e| (e.client_status(), e.to_string()))
}
