//! Customer creation and onboarding.
//!
//! Onboarding is **hosted redirect only**. We send Dakota a name, a type and
//! our own reference, and get back an `application_url`; the customer completes
//! beneficial owners, documents, SSNs and attestations on Dakota's form. None
//! of it passes through this service, which is what makes the no-PII policy
//! real rather than aspirational.
//!
//! What we persist is the skeleton: ids, type, hierarchy and status. Names are
//! relayed from Dakota per-request and never written down.

use std::sync::Arc;

use auth_client::VerifiedClaims;
use axum::extract::{Path, State};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use tracing::info;

use super::{internal, ApiError};
use crate::authz::{authorize_customer, creation_sub_client, Caller};
use crate::dakota::types::*;
use crate::db::models::{Customer, UpsertCustomer};
use crate::invites::Invite;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct CreateCustomerBody {
    pub name: String,
    /// `business` | `individual`.
    pub customer_type: String,
    #[serde(default)]
    pub external_ref: Option<String>,
    /// Make this customer a partner business with its own roster beneath it.
    /// Immutable after creation, and mutually exclusive with `sub_client_id`.
    #[serde(default)]
    pub is_sub_client: bool,
    /// File this customer under a partner business. Ignored for business
    /// callers, who may only create beneath themselves.
    #[serde(default)]
    pub sub_client_id: Option<String>,
    /// Also mint a signup link so the customer can reach their own dashboard.
    #[serde(default)]
    pub with_invite: bool,
}

#[derive(Serialize)]
pub struct CreateCustomerResult {
    pub customer: Customer,
    /// Dakota's hosted onboarding form. Send the customer here — everything
    /// sensitive is collected on the far side.
    pub application_url: String,
    /// Signup grant for our dashboard, when asked for. The dashboard turns
    /// this into `/signup?invite=…`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invite: Option<Invite>,
}

/// `POST /customers`
pub async fn create_customer(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
    Json(body): Json<CreateCustomerBody>,
) -> Result<Json<CreateCustomerResult>, ApiError> {
    let caller = Caller::from_claims(&claims)?;

    if body.is_sub_client && !caller.is_admin() {
        return Err(super::bad_request("only an admin can create a partner business"));
    }
    // Taken from the caller's token, not the body — this is what stops one
    // business filing customers under another.
    let sub_client_id = creation_sub_client(&caller, body.sub_client_id.as_deref())?;
    if body.is_sub_client && sub_client_id.is_some() {
        return Err(super::bad_request(
            "is_sub_client and sub_client_id are mutually exclusive",
        ));
    }

    let created: CreateCustomerResp = state
        .dakota
        .post(
            "POST /customers",
            "/customers",
            &CreateCustomerReq {
                name: body.name,
                customer_type: body.customer_type.clone(),
                external_id: body.external_ref.clone(),
                is_sub_client: body.is_sub_client.then_some(true),
                sub_client_id: sub_client_id.clone(),
            },
        )
        .await
        .map_err(|e| (e.client_status(), e.to_string()))?;

    let customer = state
        .repo
        .upsert_customer(&UpsertCustomer {
            dakota_customer_id: created.id.clone(),
            customer_type: body.customer_type,
            is_sub_client: body.is_sub_client,
            sub_client_id,
            external_ref: body.external_ref,
            application_id: Some(created.application_id.clone()),
            // Dakota starts everyone here; the real value arrives by webhook.
            kyb_status: Some("pending".into()),
            kyc_status: Some("not_started".into()),
            application_status: Some("not_started".into()),
        })
        .map_err(internal)?;

    let invite = if body.with_invite {
        let role = if body.is_sub_client { "business" } else { "individual" };
        Some(
            state
                .invites
                .mint(role, Some(&created.id), Some(&format!("{role} onboarding")))
                .await
                .map_err(internal)?,
        )
    } else {
        None
    };

    info!(
        customer_id = %created.id,
        is_sub_client = body.is_sub_client,
        "customer created; handing off to hosted onboarding"
    );
    Ok(Json(CreateCustomerResult {
        customer,
        application_url: created.application_url,
        invite,
    }))
}

/// `GET /customers` — the roster this caller may see.
///
/// An individual sees only itself; a business sees its own customers; an admin
/// sees everyone.
pub async fn list_customers(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
) -> Result<Json<Vec<Customer>>, ApiError> {
    let caller = Caller::from_claims(&claims)?;
    let rows = match &caller {
        Caller::Individual { customer_id } => state
            .repo
            .get_customer(customer_id)
            .map_err(internal)?
            .into_iter()
            .collect(),
        _ => state
            .repo
            .list_customers(caller.sub_client_filter())
            .map_err(internal)?,
    };
    Ok(Json(rows))
}

/// `GET /admin/sub-clients` — partner businesses, with Dakota's own rollup.
pub async fn list_sub_clients(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Caller::from_claims(&claims)?.require_admin()?;

    let local = state.repo.list_sub_clients().map_err(internal)?;
    // Dakota's summary carries `sub_client_name` — relayed for display, never
    // stored.
    let remote: serde_json::Value = state
        .dakota
        .get(
            "GET /customers/sub-client-summary",
            "/customers/sub-client-summary",
        )
        .await
        .map_err(|e| (e.client_status(), e.to_string()))?;

    Ok(Json(serde_json::json!({ "sub_clients": local, "summary": remote })))
}

/// `GET /customers/:id` — live detail, straight from Dakota.
///
/// Returns Dakota's body untouched (including `name`, which we never store) so
/// the dashboard can display a human-readable record without us keeping one.
pub async fn get_customer(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
    Path(customer_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let caller = Caller::from_claims(&claims)?;
    let local = authorize_customer(&state, &caller, &customer_id)?;

    let remote: serde_json::Value = state
        .dakota
        .get("GET /customers/{id}", &format!("/customers/{customer_id}"))
        .await
        .map_err(|e| (e.client_status(), e.to_string()))?;

    // Refresh our status skeleton off the authoritative copy while we have it.
    if let Ok(status) = serde_json::from_value::<CustomerStatus>(remote.clone()) {
        let _ = state.repo.upsert_customer(&UpsertCustomer {
            dakota_customer_id: local.dakota_customer_id.clone(),
            customer_type: local.customer_type.clone(),
            is_sub_client: local.is_sub_client,
            sub_client_id: local.sub_client_id.clone(),
            external_ref: local.external_ref.clone(),
            application_id: status.application_id.clone(),
            kyb_status: status.kyb_status.clone(),
            kyc_status: status.kyc_status.clone(),
            application_status: status.application_status.clone(),
        });
    }

    Ok(Json(remote))
}

/// `GET /customers/:id/capabilities` — what this customer can do and what is
/// still blocking them. Dakota returns requirement rows with a hosted `url`
/// per item, which is exactly what the dashboard's "finish onboarding" panel
/// links to.
pub async fn get_capabilities(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
    Path(customer_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let caller = Caller::from_claims(&claims)?;
    authorize_customer(&state, &caller, &customer_id)?;

    state
        .dakota
        .get(
            "GET /customers/{id}/capabilities",
            &format!("/customers/{customer_id}/capabilities"),
        )
        .await
        .map(Json)
        .map_err(|e| (e.client_status(), e.to_string()))
}

/// `POST /customers/:id/invite` — mint a fresh signup link.
///
/// A business uses this to onboard its own customers; an admin for anyone.
pub async fn create_invite(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
    Path(customer_id): Path<String>,
) -> Result<Json<Invite>, ApiError> {
    let caller = Caller::from_claims(&claims)?;
    let customer = authorize_customer(&state, &caller, &customer_id)?;
    if matches!(caller, Caller::Individual { .. }) {
        return Err((
            axum::http::StatusCode::FORBIDDEN,
            "individuals cannot mint invites".into(),
        ));
    }

    let role = if customer.is_sub_client { "business" } else { "individual" };
    state
        .invites
        .mint(role, Some(&customer_id), Some(&format!("{role} onboarding")))
        .await
        .map(Json)
        .map_err(internal)
}
