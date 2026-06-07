//! Client + axum middleware for the **auth-service** (`services/auth-service`).
//!
//! auth-service is the single holder of the JWT signing secret. Any service
//! that wants to gate an endpoint on a valid admin JWT does NOT verify the
//! token itself — it calls [`AuthClient::verify`], which delegates to
//! auth-service's internal `/verify` route. This keeps the secret in exactly
//! one place (auth-service) per the trust model: token-info and friends never
//! learn how to validate a JWT, they only learn the yes/no answer.
//!
//! ## Usage
//!
//! ```ignore
//! let auth = std::sync::Arc::new(AuthClient::new("http://auth-service:9008"));
//! let protected = Router::new()
//!     .route("/tokens", post(create))
//!     .route_layer(axum::middleware::from_fn_with_state(auth, require_auth));
//! ```
//!
//! On success the verified [`VerifiedClaims`] are inserted into the request
//! extensions, so downstream handlers can read the caller's address with
//! `Extension<VerifiedClaims>`.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// The claims auth-service confirms for a valid token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedClaims {
    /// Sui address the token was issued to (the admin), `0x`-prefixed.
    pub address: String,
    /// Expiry, unix seconds.
    pub exp: u64,
}

/// Wire shape of auth-service's `POST /verify` response.
#[derive(Debug, Deserialize)]
struct VerifyResp {
    valid: bool,
    #[serde(default)]
    address: Option<String>,
    #[serde(default)]
    exp: Option<u64>,
}

/// HTTP client for auth-service's internal verify route.
#[derive(Debug, Clone)]
pub struct AuthClient {
    base_url: String,
    http: reqwest::Client,
}

impl AuthClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// Ask auth-service whether `token` is a currently-valid admin JWT.
    /// Returns the verified claims on success; `Ok(None)` if auth-service
    /// reports the token invalid; `Err` only on a transport/upstream failure.
    pub async fn verify(&self, token: &str) -> anyhow::Result<Option<VerifiedClaims>> {
        let url = format!("{}/verify", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({ "token": token }))
            .send()
            .await?
            .error_for_status()?
            .json::<VerifyResp>()
            .await?;
        if resp.valid {
            Ok(Some(VerifiedClaims {
                address: resp.address.unwrap_or_default(),
                exp: resp.exp.unwrap_or_default(),
            }))
        } else {
            Ok(None)
        }
    }
}

/// Pull the bearer token out of the `Authorization` header.
fn bearer(req: &Request) -> Option<String> {
    let raw = req.headers().get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    raw.strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .map(|s| s.trim().to_string())
}

/// axum middleware: require a valid admin JWT, verified by auth-service.
///
/// Wire with `from_fn_with_state(Arc::new(AuthClient::new(url)), require_auth)`.
/// 401 if the header is missing/invalid or the token doesn't verify; 502 if
/// auth-service is unreachable (fail closed — never let a request through when
/// we can't confirm it).
pub async fn require_auth(
    State(auth): State<Arc<AuthClient>>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(token) = bearer(&req) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    match auth.verify(&token).await {
        Ok(Some(claims)) => {
            debug!(address = %claims.address, "auth ok");
            req.extensions_mut().insert(claims);
            Ok(next.run(req).await)
        }
        Ok(None) => Err(StatusCode::UNAUTHORIZED),
        Err(e) => {
            warn!(error = %e, "auth-service verify failed; rejecting");
            Err(StatusCode::BAD_GATEWAY)
        }
    }
}
