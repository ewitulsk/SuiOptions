//! Account lifecycle: redeeming an invite, whoami, and linking login methods.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Json, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use tracing::info;
use uuid::Uuid;

use super::session::verify_challenge_signature;
use super::{bad_request, client_ip, internal, issue_token, load_user, ApiError, TokenResp};
use crate::db::models::IdentityKind;
use crate::password;
use crate::state::AppState;

// -------------------------------------------------------------- invite peek

#[derive(Deserialize)]
pub struct InviteQuery {
    pub invite: Uuid,
}

#[derive(Serialize)]
pub struct InvitePreview {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub valid: bool,
    pub reason: Option<String>,
}

/// `GET /invites/preview?invite=<uuid>` — what an invite link is for, before
/// the visitor commits to it. Deliberately leaks nothing but the role and the
/// label the minter chose; the scope id stays server-side.
pub async fn preview_invite(
    State(state): State<Arc<AppState>>,
    Query(q): Query<InviteQuery>,
) -> Result<Json<InvitePreview>, ApiError> {
    let invite = state
        .repo
        .peek_invite(q.invite)
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "unknown invite".to_string()))?;

    let reason = if invite.consumed_at.is_some() {
        Some("already used".to_string())
    } else if invite.expires_at <= chrono::Utc::now() {
        Some("expired".to_string())
    } else {
        None
    };

    Ok(Json(InvitePreview {
        role: invite.role,
        label: invite.label,
        valid: reason.is_none(),
        reason,
    }))
}

// ------------------------------------------------------------------ register

/// How the new account will log in. Untagged so the body reads naturally:
/// either `{username, password}` or `{signature, bytes}`.
#[derive(Deserialize)]
#[serde(untagged)]
pub enum RegisterMethod {
    Password { username: String, password: String },
    SuiWallet { signature: String, bytes: String },
}

#[derive(Deserialize)]
pub struct RegisterReq {
    pub invite: Uuid,
    #[serde(flatten)]
    pub method: RegisterMethod,
}

/// `POST /register` — redeem an invite into a new account and open its session.
///
/// The invite carries the role and scope; nothing about them is taken from the
/// request body, so a redeemer cannot promote themselves.
pub async fn register(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<RegisterReq>,
) -> Result<Json<TokenResp>, ApiError> {
    let (kind, identifier, secret_hash, address) = match &req.method {
        RegisterMethod::Password { username, password } => {
            let username = password::normalize_username(username);
            password::validate_username(&username).map_err(bad_request)?;
            password::validate_password(password).map_err(bad_request)?;
            let hash = password::hash(password).map_err(internal)?;
            (IdentityKind::Password, username, Some(hash), None)
        }
        RegisterMethod::SuiWallet { signature, bytes } => {
            let address = verify_challenge_signature(&state, signature, bytes)?;
            (IdentityKind::SuiWallet, address.clone(), None, Some(address))
        }
    };

    let user = state
        .repo
        .register_with_invite(req.invite, kind, &identifier, secret_hash)
        .map_err(|e| {
            // Every failure here is the caller's: a spent, expired or unknown
            // invite, or an identifier someone already claimed.
            (StatusCode::BAD_REQUEST, e.to_string())
        })?;

    metrics::counter!("auth_registrations_total", "method" => kind.as_str()).increment(1);
    info!(user_id = %user.id, role = %user.role, method = kind.as_str(), "account registered");
    Ok(Json(issue_token(&state, &user, address, client_ip(&headers, peer))?))
}

// --------------------------------------------------------------------- me

#[derive(Serialize)]
pub struct IdentityView {
    pub id: String,
    pub kind: String,
    /// Username or `0x` address. Never PII by construction — see the migration.
    pub identifier: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
}

#[derive(Serialize)]
pub struct MeResp {
    pub user_id: String,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub identities: Vec<IdentityView>,
}

/// `GET /me` — the account behind the current token, with its login methods.
pub async fn me(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<MeResp>, ApiError> {
    let claims = super::require_session(&state, &headers)?;
    let user = load_user(&state, &claims)?;
    let identities = state.repo.list_identities(user.id).map_err(internal)?;

    Ok(Json(MeResp {
        user_id: user.id.to_string(),
        role: user.role,
        scope: user.scope_id,
        identities: identities
            .into_iter()
            .map(|i| IdentityView {
                id: i.id.to_string(),
                kind: i.kind,
                identifier: i.identifier,
                created_at: i.created_at.to_rfc3339(),
                last_used_at: i.last_used_at.map(|t| t.to_rfc3339()),
            })
            .collect(),
    }))
}

// ---------------------------------------------------------- identity linking

#[derive(Deserialize)]
#[serde(untagged)]
pub enum AddIdentityReq {
    Password { username: String, password: String },
    SuiWallet { signature: String, bytes: String },
}

/// `POST /identities` — attach a second login method to the current account.
///
/// This is both directions of what the account owner asked for: a wallet
/// account setting a password, and a password account adding a wallet. A wallet
/// still has to prove itself with a fresh signed challenge; asserting an
/// address would let anyone graft their session onto someone else's wallet.
pub async fn add_identity(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<AddIdentityReq>,
) -> Result<Json<IdentityView>, ApiError> {
    let claims = super::require_session(&state, &headers)?;
    let user = load_user(&state, &claims)?;

    let (kind, identifier, secret_hash) = match &req {
        AddIdentityReq::Password { username, password } => {
            let username = password::normalize_username(username);
            password::validate_username(&username).map_err(bad_request)?;
            password::validate_password(password).map_err(bad_request)?;
            let hash = password::hash(password).map_err(internal)?;
            (IdentityKind::Password, username, Some(hash))
        }
        AddIdentityReq::SuiWallet { signature, bytes } => {
            let address = verify_challenge_signature(&state, signature, bytes)?;
            (IdentityKind::SuiWallet, address, None)
        }
    };

    // The UNIQUE (kind, identifier) index is what actually stops a takeover:
    // a wallet already bound elsewhere cannot be bound here.
    let identity = state
        .repo
        .add_identity(user.id, kind, &identifier, secret_hash)
        .map_err(|_| {
            (
                StatusCode::CONFLICT,
                "that login method is already in use".to_string(),
            )
        })?;

    info!(user_id = %user.id, method = kind.as_str(), "identity linked");
    Ok(Json(IdentityView {
        id: identity.id.to_string(),
        kind: identity.kind,
        identifier: identity.identifier,
        created_at: identity.created_at.to_rfc3339(),
        last_used_at: None,
    }))
}

/// `DELETE /identities/:id` — drop a login method. The repo refuses the last
/// one; with no email on file there would be no way back into the account.
pub async fn remove_identity(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(identity_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let claims = super::require_session(&state, &headers)?;
    let user = load_user(&state, &claims)?;
    state
        .repo
        .remove_identity(user.id, identity_id)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    info!(user_id = %user.id, %identity_id, "identity removed");
    Ok(StatusCode::NO_CONTENT)
}
