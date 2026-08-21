//! HTTP handlers.
//!
//! Public: [`challenge`], [`login`], [`refresh`].
//! Internal: [`verify`] (delegated to by `auth-client`).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Json, State};
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::allowlist;
use crate::jwt::{self, Claims};
use crate::state::AppState;
use crate::sui_sig;

type ApiError = (StatusCode, String);

pub async fn health() -> &'static str {
    "ok"
}

// ----------------------------------------------------------------- challenge

#[derive(Serialize)]
pub struct ChallengeResp {
    /// Exact message the wallet must sign (UTF-8).
    pub message: String,
}

/// `GET /challenge` — mint a single-use message to sign.
pub async fn challenge(State(state): State<Arc<AppState>>) -> Json<ChallengeResp> {
    metrics::counter!("auth_challenges_issued_total").increment(1);
    Json(ChallengeResp {
        message: state.challenges.issue(),
    })
}

// --------------------------------------------------------------------- login

#[derive(Deserialize)]
pub struct LoginReq {
    /// Base64 serialized Sui signature (`signPersonalMessage().signature`).
    pub signature: String,
    /// Base64 of the signed message (`signPersonalMessage().bytes`).
    pub bytes: String,
}

#[derive(Serialize)]
pub struct TokenResp {
    pub token: String,
    pub address: String,
    /// Seconds until the token expires.
    pub expires_in: u64,
}

/// `POST /login` — verify a signed challenge against the allowlist, issue a JWT
/// bound to the caller's IP.
pub async fn login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<LoginReq>,
) -> Result<Json<TokenResp>, ApiError> {
    let res = login_inner(state, peer, headers, req).await;
    let outcome = match &res {
        Ok(_) => "ok",
        Err((code, _)) if code.is_server_error() => "error",
        Err(_) => "rejected",
    };
    metrics::counter!("auth_logins_total", "outcome" => outcome).increment(1);
    res
}

async fn login_inner(
    state: Arc<AppState>,
    peer: SocketAddr,
    headers: HeaderMap,
    req: LoginReq,
) -> Result<Json<TokenResp>, ApiError> {
    use base64::Engine;

    let message = base64::engine::general_purpose::STANDARD
        .decode(req.bytes.trim())
        .map_err(|_| (StatusCode::BAD_REQUEST, "bytes is not base64".into()))?;
    let message_str = String::from_utf8(message.clone())
        .map_err(|_| (StatusCode::BAD_REQUEST, "message is not utf-8".into()))?;

    // Single-use challenge: must match one we issued and not yet consumed.
    if !state.challenges.consume(&message_str) {
        return Err((StatusCode::BAD_REQUEST, "unknown or expired challenge".into()));
    }

    let address = sui_sig::recover_and_verify(&req.signature, &message)
        .map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()))?;

    if !allowlist::is_allowed(&state.admin_addresses, &address) {
        // Not on the static list — fall back to on-chain AdminCap ownership
        // (SO-422). Any failure on this path (token-info down, GraphQL down)
        // is a plain rejection: never fail open.
        let holds_cap = match &state.cap_check {
            Some(check) => check.holds_admin_cap(&address).await.unwrap_or_else(|e| {
                warn!(%address, error = %e, "AdminCap check failed; treating as not held");
                false
            }),
            None => false,
        };
        if !holds_cap {
            warn!(%address, "login rejected: not on allowlist and no AdminCap");
            return Err((StatusCode::FORBIDDEN, "address not on admin allowlist".into()));
        }
        info!(%address, "login allowed via on-chain AdminCap");
        metrics::counter!("auth_logins_capcheck_total").increment(1);
    }

    let now = jwt::now_secs();
    let claims = Claims {
        sub: allowlist::normalize(&address),
        ip: client_ip(&headers, peer),
        iat: now,
        exp: now + state.token_ttl_secs,
    };
    let token = jwt::sign(&claims, &state.jwt_secret)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    info!(address = %claims.sub, "admin login");
    Ok(Json(TokenResp {
        token,
        address: claims.sub,
        expires_in: state.token_ttl_secs,
    }))
}

// ------------------------------------------------------------------- refresh

/// `POST /refresh` — issue a fresh token from a still-in-window one, provided
/// the request comes from the same IP. No re-signing required.
pub async fn refresh(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<TokenResp>, ApiError> {
    let token = bearer(&headers)
        .ok_or((StatusCode::UNAUTHORIZED, "missing bearer token".into()))?;

    // Accept an expired-but-otherwise-valid token (exp not enforced here); the
    // refresh window is bounded by the original iat below.
    let claims = jwt::verify(&token, &state.jwt_secret, false)
        .map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()))?;

    let now = jwt::now_secs();
    if now >= claims.iat + state.refresh_max_secs {
        return Err((StatusCode::UNAUTHORIZED, "refresh window elapsed; sign in again".into()));
    }
    if client_ip(&headers, peer) != claims.ip {
        return Err((StatusCode::UNAUTHORIZED, "refresh must come from the same IP".into()));
    }

    // Preserve the original iat so the total session stays bounded by
    // refresh_max_secs; only the expiry slides forward.
    let next = Claims {
        sub: claims.sub.clone(),
        ip: claims.ip,
        iat: claims.iat,
        exp: now + state.token_ttl_secs,
    };
    let token = jwt::sign(&next, &state.jwt_secret)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(TokenResp {
        token,
        address: next.sub,
        expires_in: state.token_ttl_secs,
    }))
}

// -------------------------------------------------------------------- verify

#[derive(Deserialize)]
pub struct VerifyReq {
    pub token: String,
}

#[derive(Serialize)]
pub struct VerifyResp {
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exp: Option<u64>,
}

/// `POST /verify` (internal) — the yes/no answer other services delegate to.
pub async fn verify(
    State(state): State<Arc<AppState>>,
    Json(req): Json<VerifyReq>,
) -> Json<VerifyResp> {
    match jwt::verify(&req.token, &state.jwt_secret, true) {
        Ok(claims) => {
            metrics::counter!("auth_verifies_total", "outcome" => "ok").increment(1);
            Json(VerifyResp {
                valid: true,
                address: Some(claims.sub),
                exp: Some(claims.exp),
            })
        }
        Err(_) => {
            metrics::counter!("auth_verifies_total", "outcome" => "invalid").increment(1);
            Json(VerifyResp { valid: false, address: None, exp: None })
        }
    }
}

// --------------------------------------------------------------------- utils

fn bearer(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    raw.strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .map(|s| s.trim().to_string())
}

/// Client IP, preferring the left-most `X-Forwarded-For` entry (set by nginx)
/// and falling back to the direct peer for local/dev calls.
fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| peer.ip().to_string())
}
