//! Narrow Bluefin REST pass-through for the curator dashboard (SO-305).
//!
//! Bluefin's API serves CORS headers only to origins on THEIR allowlist
//! (their own web app) — a third-party dashboard origin is blocked by the
//! browser. The dashboard therefore reaches Bluefin through this relay,
//! which already serves CORS to the dashboard origin via the service-wide
//! `allowed_origins` layer.
//!
//! Custody posture: the proxy holds no keys and signs nothing. Every
//! privileged payload it forwards (order / withdraw / authorize / login)
//! was signed client-side — by the curator's wallet or by the FROST
//! ceremony whose service half sits behind the `/frost/*` policy engine —
//! and Bluefin verifies those signatures itself. The proxy only narrows
//! the reachable surface: requests outside the fixed method+path allowlist
//! below are refused, so it cannot be used as an open relay.
//!
//! Routes: `/bluefin/{auth|data|trade}/<path>` → the corresponding
//! configured base URL (`[bluefin_proxy]` in config). No config ⇒ 503.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use tracing::{info, warn};

use crate::config::BluefinProxyConfig;

/// Forwarded request headers: auth token + the detached payload signature
/// Bluefin's login endpoint reads (`payloadSignature`) + content type.
const FORWARD_HEADERS: [&str; 3] = ["authorization", "payloadsignature", "content-type"];

/// (host slot, method, exact path) triples the proxy will forward.
/// Everything else is refused — the allowlist IS the product.
const ALLOWLIST: &[(&str, &str, &str)] = &[
    // Auth host: token minting (payload signed client-side) + refresh.
    ("auth", "POST", "/auth/v2/token"),
    ("auth", "PUT", "/auth/token/refresh"),
    // Data host: public market/contract info and account reads.
    ("data", "GET", "/v1/exchange/info"),
    ("data", "GET", "/api/v1/account"),
    ("data", "GET", "/api/v1/account/trades"),
    ("data", "GET", "/api/v1/account/transactions"),
    // Trade host: order relay (payloads signed by the curator wallet),
    // cancel (JWT-only per Bluefin's API), withdraw/authorize (payloads
    // signed by the FROST ceremony behind /frost policy).
    ("trade", "GET", "/api/v1/trade/openOrders"),
    ("trade", "POST", "/api/v1/trade/orders"),
    ("trade", "PUT", "/api/v1/trade/orders/cancel"),
    ("trade", "POST", "/api/v1/trade/withdraw"),
    ("trade", "PUT", "/api/v1/trade/accounts/authorize"),
    ("trade", "PUT", "/api/v1/trade/accounts/deauthorize"),
];

pub struct BluefinProxy {
    cfg: Option<BluefinProxyConfig>,
    client: reqwest::Client,
}

impl BluefinProxy {
    pub fn new(cfg: Option<BluefinProxyConfig>) -> Self {
        Self {
            cfg,
            client: reqwest::Client::new(),
        }
    }

    fn base_url(&self, host: &str) -> Option<&str> {
        let cfg = self.cfg.as_ref()?;
        match host {
            "auth" => Some(&cfg.auth_base_url),
            "data" => Some(&cfg.api_base_url),
            "trade" => Some(&cfg.trade_base_url),
            _ => None,
        }
    }
}

/// Is (host, method, path) on the forwarding allowlist?
pub fn allowed(host: &str, method: &str, path: &str) -> bool {
    ALLOWLIST
        .iter()
        .any(|(h, m, p)| *h == host && *m == method && *p == path)
}

pub fn router(proxy: Arc<BluefinProxy>) -> Router {
    Router::new()
        .route("/bluefin/:host/*path", any(forward))
        .with_state(proxy)
}

async fn forward(
    State(proxy): State<Arc<BluefinProxy>>,
    Path((host, path)): Path<(String, String)>,
    Query(query): Query<Vec<(String, String)>>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/{path}");
    if proxy.cfg.is_none() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "bluefin proxy is not configured on this deployment",
        )
            .into_response();
    }
    let Some(base) = proxy.base_url(&host) else {
        return (
            StatusCode::FORBIDDEN,
            format!("unknown bluefin host slot {host:?}"),
        )
            .into_response();
    };
    if !allowed(&host, method.as_str(), &path) {
        warn!(%host, %method, %path, "refusing bluefin proxy request outside the allowlist");
        return (
            StatusCode::FORBIDDEN,
            format!("{method} {path} is not in the bluefin proxy allowlist"),
        )
            .into_response();
    }

    let mut req = proxy
        .client
        .request(method.clone(), format!("{base}{path}"))
        .query(&query)
        .body(body);
    for name in FORWARD_HEADERS {
        if let Some(v) = headers.get(name) {
            req = req.header(name, v);
        }
    }

    match req.send().await {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_string();
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    return (
                        StatusCode::BAD_GATEWAY,
                        format!("bluefin response read: {e}"),
                    )
                        .into_response()
                }
            };
            info!(%host, %path, status = status.as_u16(), "bluefin proxy forwarded");
            (status, [("content-type", content_type)], bytes).into_response()
        }
        Err(e) => {
            warn!(%host, %path, error = %e, "bluefin proxy upstream request failed");
            (StatusCode::BAD_GATEWAY, format!("bluefin upstream: {e}")).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_admits_the_dashboard_surface() {
        assert!(allowed("auth", "POST", "/auth/v2/token"));
        assert!(allowed("data", "GET", "/api/v1/account"));
        assert!(allowed("trade", "POST", "/api/v1/trade/orders"));
        assert!(allowed("trade", "PUT", "/api/v1/trade/orders/cancel"));
        assert!(allowed("trade", "POST", "/api/v1/trade/withdraw"));
        assert!(allowed("trade", "PUT", "/api/v1/trade/accounts/authorize"));
    }

    #[test]
    fn allowlist_refuses_everything_else() {
        // Wrong method.
        assert!(!allowed("auth", "GET", "/auth/v2/token"));
        // Wrong host slot for an allowed path.
        assert!(!allowed("data", "POST", "/api/v1/trade/orders"));
        // Paths that must never be reachable through the relay.
        assert!(!allowed("trade", "PUT", "/api/v1/trade/leverage"));
        assert!(!allowed("data", "POST", "/api/v1/account/sponsorTx"));
        assert!(!allowed("auth", "GET", "/auth/jwks"));
        // Path traversal / prefix tricks: exact match only.
        assert!(!allowed(
            "trade",
            "POST",
            "/api/v1/trade/orders/../../admin"
        ));
        assert!(!allowed("trade", "POST", "/api/v1/trade/orders2"));
    }
}
