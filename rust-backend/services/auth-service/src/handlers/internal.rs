//! Internal-port handlers. Bound on a port nginx never proxies and reachable
//! only container-to-container or over the VPN — there is no caller
//! authentication here, so the network boundary is the whole control.

use std::sync::Arc;

use axum::extract::{Json, State};
use serde::{Deserialize, Serialize};
use tracing::info;
use uuid::Uuid;

use super::{internal, parse_role, ApiError};
use crate::jwt;
use crate::state::AppState;

// -------------------------------------------------------------------- verify

#[derive(Deserialize)]
pub struct VerifyReq {
    pub token: String,
}

#[derive(Serialize)]
pub struct VerifyResp {
    pub valid: bool,
    /// Sui address, when the session was opened by wallet. Kept under this name
    /// for `auth-client`'s existing `VerifiedClaims`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exp: Option<u64>,
}

/// `POST /verify` — the yes/no answer other services delegate to, now carrying
/// the role and scope so callers can authorize as well as authenticate.
///
/// Claims are read from the token rather than the database: this is on the hot
/// path of every gated request in the fleet, and a role change lands at the
/// next refresh (at most `token_ttl_secs` away).
pub async fn verify(
    State(state): State<Arc<AppState>>,
    Json(req): Json<VerifyReq>,
) -> Json<VerifyResp> {
    match jwt::verify(&req.token, &state.jwt_secret, true) {
        Ok(claims) => {
            metrics::counter!("auth_verifies_total", "outcome" => "ok").increment(1);
            Json(VerifyResp {
                valid: true,
                address: claims.address,
                user_id: Some(claims.sub),
                role: Some(claims.role),
                scope: claims.scope,
                exp: Some(claims.exp),
            })
        }
        Err(_) => {
            metrics::counter!("auth_verifies_total", "outcome" => "invalid").increment(1);
            Json(VerifyResp {
                valid: false,
                address: None,
                user_id: None,
                role: None,
                scope: None,
                exp: None,
            })
        }
    }
}

// ------------------------------------------------------------------- invites

#[derive(Deserialize)]
pub struct CreateInviteReq {
    /// `admin` | `business` | `individual`.
    pub role: String,
    /// Required for every role but `admin`.
    #[serde(default)]
    pub scope_id: Option<String>,
    /// Shown on the signup page so the invitee knows what they are joining.
    /// Keep it non-identifying — it is the one free-text field here.
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub created_by: Option<Uuid>,
    /// Override the configured default lifetime.
    #[serde(default)]
    pub ttl_secs: Option<i64>,
}

#[derive(Serialize)]
pub struct CreateInviteResp {
    pub invite_id: String,
    pub role: String,
    pub expires_at: String,
}

/// `POST /invites` — mint a signup grant. dakota-service calls this when an
/// admin creates a partner business, or when a business invites one of its own
/// customers.
pub async fn create_invite(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateInviteReq>,
) -> Result<Json<CreateInviteResp>, ApiError> {
    let role = parse_role(&req.role)?;
    let ttl = req.ttl_secs.unwrap_or(state.invite_ttl_secs);

    let invite = state
        .repo
        .create_invite(role, req.scope_id, req.created_by, req.label, ttl)
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e.to_string()))?;

    info!(invite_id = %invite.id, role = %invite.role, "invite minted");
    Ok(Json(CreateInviteResp {
        invite_id: invite.id.to_string(),
        role: invite.role,
        expires_at: invite.expires_at.to_rfc3339(),
    }))
}

/// `GET /health` on the internal port doubles as the readiness probe, so it
/// touches the database rather than answering blind.
pub async fn ready(State(state): State<Arc<AppState>>) -> Result<&'static str, ApiError> {
    state
        .repo
        .peek_invite(Uuid::nil())
        .map(|_| "ok")
        .map_err(internal)
}
