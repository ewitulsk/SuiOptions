//! HTTP handlers.
//!
//! - [`session`] — public: challenge, wallet login, password login, refresh.
//! - [`account`] — public, authenticated: register, whoami, identity linking.
//! - [`internal`] — internal port: token verification and invite minting.

pub mod account;
pub mod internal;
pub mod session;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::{HeaderMap, StatusCode};
use serde::Serialize;

use crate::db::models::{Role, User};
use crate::jwt::{self, Claims};
use crate::state::AppState;

pub type ApiError = (StatusCode, String);

pub async fn health() -> &'static str {
    "ok"
}

/// What every successful authentication returns.
#[derive(Serialize)]
pub struct TokenResp {
    pub token: String,
    pub user_id: String,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Present only for wallet-opened sessions. Retained under this name
    /// because the existing admin frontend reads it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// Seconds until the token expires.
    pub expires_in: u64,
}

/// Mint a session token for `user`.
pub fn issue_token(
    state: &AppState,
    user: &User,
    address: Option<String>,
    ip: String,
) -> Result<TokenResp, ApiError> {
    let now = jwt::now_secs();
    let claims = Claims {
        sub: user.id.to_string(),
        role: user.role.clone(),
        scope: user.scope_id.clone(),
        address,
        ip,
        iat: now,
        exp: now + state.token_ttl_secs,
    };
    let token = jwt::sign(&claims, &state.jwt_secret).map_err(internal)?;
    Ok(TokenResp {
        token,
        user_id: claims.sub,
        role: claims.role,
        scope: claims.scope,
        address: claims.address,
        expires_in: state.token_ttl_secs,
    })
}

/// Require a valid, unexpired session on a public route, returning its claims.
pub fn require_session(state: &AppState, headers: &HeaderMap) -> Result<Claims, ApiError> {
    let token = bearer(headers).ok_or((StatusCode::UNAUTHORIZED, "missing bearer token".into()))?;
    jwt::verify(&token, &state.jwt_secret, true)
        .map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()))
}

/// Load the account behind a set of claims, rejecting disabled ones.
pub fn load_user(state: &Arc<AppState>, claims: &Claims) -> Result<User, ApiError> {
    let user_id = claims
        .sub
        .parse()
        .map_err(|_| (StatusCode::UNAUTHORIZED, "malformed subject".to_string()))?;
    let user = state
        .repo
        .get_user(user_id)
        .map_err(internal)?
        .ok_or((StatusCode::UNAUTHORIZED, "account no longer exists".to_string()))?;
    if user.disabled_at.is_some() {
        return Err((StatusCode::FORBIDDEN, "account disabled".into()));
    }
    Ok(user)
}

/// Parse a stored role, treating an unknown value as a server fault rather
/// than silently downgrading the caller's authority.
pub fn parse_role(raw: &str) -> Result<Role, ApiError> {
    Role::parse(raw).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

pub fn internal<E: std::fmt::Display>(e: E) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

pub fn bad_request<E: std::fmt::Display>(e: E) -> ApiError {
    (StatusCode::BAD_REQUEST, e.to_string())
}

/// Pull the bearer token out of the `Authorization` header.
pub fn bearer(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    raw.strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .map(|s| s.trim().to_string())
}

/// Client IP, preferring the left-most `X-Forwarded-For` entry (set by nginx)
/// and falling back to the direct peer for local/dev calls.
pub fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| peer.ip().to_string())
}
