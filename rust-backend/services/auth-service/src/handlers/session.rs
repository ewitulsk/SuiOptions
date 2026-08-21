//! Opening a session: challenge, wallet login, password login, refresh.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Json, State};
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::{client_ip, internal, issue_token, ApiError, TokenResp};
use crate::allowlist;
use crate::db::models::{IdentityKind, Role};
use crate::jwt::{self, Claims};
use crate::password;
use crate::state::AppState;
use crate::sui_sig;

// ----------------------------------------------------------------- challenge

#[derive(Serialize)]
pub struct ChallengeResp {
    /// Exact message the wallet must sign (UTF-8).
    pub message: String,
}

/// `GET /challenge` — mint a single-use message to sign. Used by both wallet
/// login and by attaching a wallet to an existing account.
pub async fn challenge(State(state): State<Arc<AppState>>) -> Json<ChallengeResp> {
    metrics::counter!("auth_challenges_issued_total").increment(1);
    Json(ChallengeResp {
        message: state.challenges.issue(),
    })
}

// --------------------------------------------------------------- wallet login

#[derive(Deserialize)]
pub struct WalletLoginReq {
    /// Base64 serialized Sui signature (`signPersonalMessage().signature`).
    pub signature: String,
    /// Base64 of the signed message (`signPersonalMessage().bytes`).
    pub bytes: String,
}

/// `POST /login` — prove control of a Sui address, then open its session.
///
/// Two ways through: the address already has a `sui_wallet` identity, or it is
/// on the config allowlist and gets auto-provisioned as an admin. Anything else
/// is rejected — a proved signature alone is not an account.
pub async fn login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<WalletLoginReq>,
) -> Result<Json<TokenResp>, ApiError> {
    let res = wallet_login_inner(state, peer, headers, req).await;
    record_login("sui_wallet", &res);
    res
}

async fn wallet_login_inner(
    state: Arc<AppState>,
    peer: SocketAddr,
    headers: HeaderMap,
    req: WalletLoginReq,
) -> Result<Json<TokenResp>, ApiError> {
    let address = verify_challenge_signature(&state, &req.signature, &req.bytes)?;

    let existing = state
        .repo
        .find_identity(IdentityKind::SuiWallet, &address)
        .map_err(internal)?;

    let user = match existing {
        Some(resolved) => {
            state.repo.touch_identity(resolved.identity.id).map_err(internal)?;
            resolved.user
        }
        None => {
            // No identity yet. Only the allowlist can conjure an account.
            if !allowlist::is_allowed(&state.admin_addresses, &address) {
                warn!(%address, "wallet login rejected: no identity and not on the admin allowlist");
                return Err((
                    StatusCode::FORBIDDEN,
                    "this wallet has no account; ask an admin for an invite".into(),
                ));
            }
            info!(%address, "bootstrapping allowlisted admin account");
            state
                .repo
                .create_user_with_identity(Role::Admin, None, IdentityKind::SuiWallet, &address, None)
                .map_err(internal)?
        }
    };

    if user.disabled_at.is_some() {
        return Err((StatusCode::FORBIDDEN, "account disabled".into()));
    }

    info!(user_id = %user.id, role = %user.role, %address, "wallet login");
    Ok(Json(issue_token(
        &state,
        &user,
        Some(address),
        client_ip(&headers, peer),
    )?))
}

// ------------------------------------------------------------ password login

#[derive(Deserialize)]
pub struct PasswordLoginReq {
    pub username: String,
    pub password: String,
}

/// `POST /login/password` — username + password.
///
/// An unknown username and a wrong password return the same 401. Enumerating
/// which usernames exist is a free gift to an attacker and costs us nothing to
/// withhold.
pub async fn login_password(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<PasswordLoginReq>,
) -> Result<Json<TokenResp>, ApiError> {
    let res = password_login_inner(state, peer, headers, req).await;
    record_login("password", &res);
    res
}

async fn password_login_inner(
    state: Arc<AppState>,
    peer: SocketAddr,
    headers: HeaderMap,
    req: PasswordLoginReq,
) -> Result<Json<TokenResp>, ApiError> {
    const REJECT: &str = "invalid username or password";

    let username = password::normalize_username(&req.username);
    let resolved = state
        .repo
        .find_identity(IdentityKind::Password, &username)
        .map_err(internal)?;

    let Some(resolved) = resolved else {
        // Hash anyway so a missing account and a wrong password take
        // comparable time — otherwise response latency enumerates usernames.
        let _ = password::hash(&req.password);
        return Err((StatusCode::UNAUTHORIZED, REJECT.into()));
    };

    let stored = resolved.identity.secret_hash.as_deref().unwrap_or_default();
    if !password::verify(&req.password, stored) {
        return Err((StatusCode::UNAUTHORIZED, REJECT.into()));
    }
    if resolved.user.disabled_at.is_some() {
        return Err((StatusCode::FORBIDDEN, "account disabled".into()));
    }

    state.repo.touch_identity(resolved.identity.id).map_err(internal)?;
    info!(user_id = %resolved.user.id, role = %resolved.user.role, "password login");
    Ok(Json(issue_token(
        &state,
        &resolved.user,
        None,
        client_ip(&headers, peer),
    )?))
}

// ------------------------------------------------------------------- refresh

/// `POST /refresh` — slide the expiry on a still-in-window token, provided the
/// request comes from the same IP. No re-signing required.
///
/// Role and scope are re-read from the database rather than copied from the old
/// token, so a role change or a disable takes effect on the next refresh
/// instead of lingering for the rest of the refresh window.
pub async fn refresh(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<TokenResp>, ApiError> {
    let token = super::bearer(&headers)
        .ok_or((StatusCode::UNAUTHORIZED, "missing bearer token".to_string()))?;

    // Accept an expired-but-otherwise-valid token; the window is bounded by the
    // original iat below.
    let claims = jwt::verify(&token, &state.jwt_secret, false)
        .map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()))?;

    let now = jwt::now_secs();
    if now >= claims.iat + state.refresh_max_secs {
        return Err((
            StatusCode::UNAUTHORIZED,
            "refresh window elapsed; sign in again".into(),
        ));
    }
    if client_ip(&headers, peer) != claims.ip {
        return Err((
            StatusCode::UNAUTHORIZED,
            "refresh must come from the same IP".into(),
        ));
    }

    let user = super::load_user(&state, &claims)?;

    // Preserve the original iat so the session stays bounded by
    // refresh_max_secs; only the expiry slides forward.
    let next = Claims {
        sub: user.id.to_string(),
        role: user.role.clone(),
        scope: user.scope_id.clone(),
        address: claims.address.clone(),
        ip: claims.ip,
        iat: claims.iat,
        exp: now + state.token_ttl_secs,
    };
    let token = jwt::sign(&next, &state.jwt_secret).map_err(internal)?;
    Ok(Json(TokenResp {
        token,
        user_id: next.sub,
        role: next.role,
        scope: next.scope,
        address: next.address,
        expires_in: state.token_ttl_secs,
    }))
}

// --------------------------------------------------------------------- utils

/// Consume a live challenge and recover the Sui address that signed it.
///
/// Shared by wallet login and wallet linking so both burn the nonce exactly
/// once — a challenge that survived verification could be replayed.
pub fn verify_challenge_signature(
    state: &AppState,
    signature: &str,
    bytes_b64: &str,
) -> Result<String, ApiError> {
    use base64::Engine;

    let message = base64::engine::general_purpose::STANDARD
        .decode(bytes_b64.trim())
        .map_err(|_| (StatusCode::BAD_REQUEST, "bytes is not base64".to_string()))?;
    let message_str = String::from_utf8(message.clone())
        .map_err(|_| (StatusCode::BAD_REQUEST, "message is not utf-8".to_string()))?;

    if !state.challenges.consume(&message_str) {
        return Err((
            StatusCode::BAD_REQUEST,
            "unknown or expired challenge".into(),
        ));
    }

    let address = sui_sig::recover_and_verify(signature, &message)
        .map_err(|e| (StatusCode::UNAUTHORIZED, e.to_string()))?;
    Ok(allowlist::normalize(&address))
}

fn record_login(method: &'static str, res: &Result<Json<TokenResp>, ApiError>) {
    let outcome = match res {
        Ok(_) => "ok",
        Err((code, _)) if code.is_server_error() => "error",
        Err(_) => "rejected",
    };
    metrics::counter!("auth_logins_total", "outcome" => outcome, "method" => method).increment(1);
}
