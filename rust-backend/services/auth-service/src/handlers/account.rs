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

// ------------------------------------------------------- credential plumbing

/// A credential the caller is presenting, either to open a new account or to
/// attach to one they already hold. Both routes take the same shape.
///
/// Tagged on `method` rather than inferred from which fields are present: an
/// untagged enum picks the first variant that deserializes and ignores unknown
/// fields, so a future variant whose body is a superset of an existing one
/// would bind to the wrong branch and silently drop the extra field. The tag
/// also turns a malformed body into a usable error instead of "data did not
/// match any variant".
#[derive(Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum AuthMethod {
    Password { username: String, password: String },
    SuiWallet { signature: String, bytes: String },
}

/// A credential validated and reduced to what the store holds. Adding a login
/// method means a variant above and an arm below; both handlers pick it up.
pub struct ResolvedMethod {
    pub kind: IdentityKind,
    /// What goes in `identities.identifier` — unique per `kind`.
    pub identifier: String,
    /// Argon2id PHC string for secret-bearing methods; `None` for methods
    /// proved by signature.
    pub secret_hash: Option<String>,
    /// Set only by methods that prove a Sui address, which then travels in the
    /// token. Ignored when linking, where the session already has one.
    pub address: Option<String>,
}

/// Validate a presented credential and hash it if it carries a secret.
///
/// Wallet methods consume the challenge nonce here, so this must be called
/// exactly once per request.
fn resolve_method(state: &AppState, method: &AuthMethod) -> Result<ResolvedMethod, ApiError> {
    Ok(match method {
        AuthMethod::Password { username, password } => {
            let username = password::normalize_username(username);
            password::validate_username(&username).map_err(bad_request)?;
            password::validate_password(password).map_err(bad_request)?;
            ResolvedMethod {
                kind: IdentityKind::Password,
                identifier: username,
                secret_hash: Some(password::hash(password).map_err(internal)?),
                address: None,
            }
        }
        AuthMethod::SuiWallet { signature, bytes } => {
            let address = verify_challenge_signature(state, signature, bytes)?;
            ResolvedMethod {
                kind: IdentityKind::SuiWallet,
                identifier: address.clone(),
                secret_hash: None,
                address: Some(address),
            }
        }
    })
}

// ------------------------------------------------------------------ register

#[derive(Deserialize)]
pub struct RegisterReq {
    pub invite: Uuid,
    #[serde(flatten)]
    pub method: AuthMethod,
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
    let resolved = resolve_method(&state, &req.method)?;

    let user = state
        .repo
        .register_with_invite(
            req.invite,
            resolved.kind,
            &resolved.identifier,
            resolved.secret_hash,
        )
        .map_err(|e| {
            // Every failure here is the caller's: a spent, expired or unknown
            // invite, or an identifier someone already claimed.
            (StatusCode::BAD_REQUEST, e.to_string())
        })?;

    metrics::counter!("auth_registrations_total", "method" => resolved.kind.as_str()).increment(1);
    info!(user_id = %user.id, role = %user.role, method = resolved.kind.as_str(), "account registered");
    Ok(Json(issue_token(
        &state,
        &user,
        resolved.address,
        client_ip(&headers, peer),
    )?))
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

/// `POST /identities` — attach a second login method to the current account.
///
/// This is both directions of what the account owner asked for: a wallet
/// account setting a password, and a password account adding a wallet. A wallet
/// still has to prove itself with a fresh signed challenge; asserting an
/// address would let anyone graft their session onto someone else's wallet.
pub async fn add_identity(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<AuthMethod>,
) -> Result<Json<IdentityView>, ApiError> {
    let claims = super::require_session(&state, &headers)?;
    let user = load_user(&state, &claims)?;

    let resolved = resolve_method(&state, &req)?;

    // The UNIQUE (kind, identifier) index is what actually stops a takeover:
    // a wallet already bound elsewhere cannot be bound here.
    let identity = state
        .repo
        .add_identity(
            user.id,
            resolved.kind,
            &resolved.identifier,
            resolved.secret_hash,
        )
        .map_err(|_| {
            (
                StatusCode::CONFLICT,
                "that login method is already in use".to_string(),
            )
        })?;

    info!(user_id = %user.id, method = resolved.kind.as_str(), "identity linked");
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

#[cfg(test)]
mod tests {
    use super::*;

    const INVITE: &str = "8f1c4a2e-0b3d-4f5a-9c6e-1d2b3a4c5d6e";

    #[test]
    fn register_body_dispatches_on_the_tag() {
        let body = serde_json::json!({
            "invite": INVITE,
            "method": "password",
            "username": "evan",
            "password": "correct horse battery",
        });
        let req: RegisterReq = serde_json::from_value(body).unwrap();
        assert_eq!(req.invite.to_string(), INVITE);
        assert!(matches!(req.method, AuthMethod::Password { .. }));

        let body = serde_json::json!({
            "invite": INVITE,
            "method": "sui_wallet",
            "signature": "sig",
            "bytes": "bytes",
        });
        let req: RegisterReq = serde_json::from_value(body).unwrap();
        assert!(matches!(req.method, AuthMethod::SuiWallet { .. }));
    }

    #[test]
    fn link_body_is_the_same_shape_minus_the_invite() {
        let body = serde_json::json!({
            "method": "sui_wallet",
            "signature": "sig",
            "bytes": "bytes",
        });
        assert!(matches!(
            serde_json::from_value::<AuthMethod>(body).unwrap(),
            AuthMethod::SuiWallet { .. }
        ));
    }

    #[test]
    fn a_body_with_no_tag_is_rejected_rather_than_guessed() {
        // The reason this enum is tagged. Untagged, serde picks the first
        // variant that happens to deserialize and drops unknown fields, so a
        // future variant overlapping this shape would silently win.
        let body = serde_json::json!({
            "invite": INVITE,
            "username": "evan",
            "password": "correct horse battery",
        });
        assert!(serde_json::from_value::<RegisterReq>(body).is_err());
    }

    #[test]
    fn an_unknown_method_names_itself_in_the_error() {
        let body = serde_json::json!({ "method": "passkey", "credential": "…" });
        // Matched rather than `unwrap_err`'d: that would need `Debug` on
        // AuthMethod, which holds a plaintext password.
        let err = match serde_json::from_value::<AuthMethod>(body) {
            Ok(_) => panic!("an unknown method was accepted"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("passkey"), "unhelpful error: {err}");
    }
}
