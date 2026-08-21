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
    /// Sui address the session was opened with, `0x`-prefixed. Empty for
    /// password sessions, which have no address — gate on [`Self::role`], not
    /// on this field.
    pub address: String,
    /// Account uuid. Stable across login methods, unlike `address`.
    pub user_id: String,
    /// `admin` | `business` | `individual`.
    pub role: String,
    /// Opaque authorization scope — dakota-service reads it as a Dakota
    /// customer id. `None` for admins, who are unscoped.
    pub scope: Option<String>,
    /// Expiry, unix seconds.
    pub exp: u64,
}

impl VerifiedClaims {
    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }
}

/// Wire shape of auth-service's `POST /verify` response.
#[derive(Debug, Deserialize)]
struct VerifyResp {
    valid: bool,
    #[serde(default)]
    address: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    scope: Option<String>,
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
        let resp = observability::client::instrumented("auth-service", "POST /verify", |h| {
            self.http
                .post(&url)
                .headers(h)
                .json(&serde_json::json!({ "token": token }))
                .send()
        })
        .await?
        .error_for_status()?
        .json::<VerifyResp>()
        .await?;
        if resp.valid {
            Ok(Some(VerifiedClaims {
                address: resp.address.unwrap_or_default(),
                user_id: resp.user_id.unwrap_or_default(),
                // Absent only if auth-service predates roles; treating that as
                // the least-privileged role fails closed.
                role: resp.role.unwrap_or_else(|| "individual".to_string()),
                scope: resp.scope,
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

/// axum middleware: require any valid JWT, verified by auth-service.
///
/// Wire with `from_fn_with_state(Arc::new(AuthClient::new(url)), require_auth)`.
/// 401 if the header is missing/invalid or the token doesn't verify; 502 if
/// auth-service is unreachable (fail closed — never let a request through when
/// we can't confirm it).
///
/// This authenticates but does NOT authorize. Since auth-service began issuing
/// tokens to non-admin roles, "valid token" no longer implies "operator" —
/// anything gating a privileged operation wants [`require_admin`].
pub async fn require_auth(
    State(auth): State<Arc<AuthClient>>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let claims = authenticate(&auth, bearer(&req)).await?;
    debug!(user_id = %claims.user_id, role = %claims.role, "auth ok");
    req.extensions_mut().insert(claims);
    Ok(next.run(req).await)
}

/// axum middleware: require a valid JWT belonging to an **admin**.
///
/// Same failure modes as [`require_auth`], plus 403 for an authenticated
/// non-admin.
pub async fn require_admin(
    State(auth): State<Arc<AuthClient>>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let claims = authenticate(&auth, bearer(&req)).await?;
    if !claims.is_admin() {
        warn!(user_id = %claims.user_id, role = %claims.role, "rejected: admin required");
        return Err(StatusCode::FORBIDDEN);
    }
    debug!(user_id = %claims.user_id, "admin auth ok");
    req.extensions_mut().insert(claims);
    Ok(next.run(req).await)
}

/// Shared verification for the middlewares above.
///
/// Takes the already-extracted token rather than the request: holding a
/// `&Request` across the `.await` would make the future non-`Send` (its `Body`
/// is not `Sync`), and axum silently rejects such a middleware with an
/// unsatisfied `Service` bound at the call site.
async fn authenticate(
    auth: &AuthClient,
    token: Option<String>,
) -> Result<VerifiedClaims, StatusCode> {
    let Some(token) = token else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    match auth.verify(&token).await {
        Ok(Some(claims)) => Ok(claims),
        Ok(None) => Err(StatusCode::UNAUTHORIZED),
        Err(e) => {
            warn!(error = %e, "auth-service verify failed; rejecting");
            Err(StatusCode::BAD_GATEWAY)
        }
    }
}
