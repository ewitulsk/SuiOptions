//! Thin typed client for the Dakota platform API.
//!
//! Two conventions are enforced here rather than at every call site, because
//! both are easy to forget and fail in confusing ways:
//!
//! - `x-api-key` on every request.
//! - `x-idempotency-key` (a fresh UUID) on **POST only**. Dakota rejects a POST
//!   without one as a 400, and rejects one *with* it on GET/PUT/PATCH/DELETE.
//!
//! Every call goes through `observability::client::instrumented`, so Dakota
//! latency and failures show up in Tempo next to the request that caused them.

use anyhow::Result;
use axum::http::StatusCode;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tracing::warn;
use uuid::Uuid;

use super::error::{DakotaError, ProblemDetails};

#[derive(Clone)]
pub struct DakotaClient {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
}

impl DakotaClient {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            http: reqwest::Client::new(),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `op` is the low-cardinality route *template* (`"GET /customers/{id}"`),
    /// not the concrete path — it becomes a metric label, and interpolating
    /// KSUIDs into it would give every customer their own time series.
    pub async fn get<R: DeserializeOwned>(
        &self,
        op: &'static str,
        path: &str,
    ) -> Result<R, DakotaError> {
        self.send(Method::GET, op, path, None::<&()>).await
    }

    pub async fn post<B: Serialize, R: DeserializeOwned>(
        &self,
        op: &'static str,
        path: &str,
        body: &B,
    ) -> Result<R, DakotaError> {
        self.send(Method::POST, op, path, Some(body)).await
    }

    pub async fn put<B: Serialize, R: DeserializeOwned>(
        &self,
        op: &'static str,
        path: &str,
        body: &B,
    ) -> Result<R, DakotaError> {
        self.send(Method::PUT, op, path, Some(body)).await
    }

    pub async fn delete<R: DeserializeOwned>(
        &self,
        op: &'static str,
        path: &str,
    ) -> Result<R, DakotaError> {
        self.send(Method::DELETE, op, path, None::<&()>).await
    }

    async fn send<B: Serialize, R: DeserializeOwned>(
        &self,
        method: Method,
        op: &'static str,
        path: &str,
        body: Option<&B>,
    ) -> Result<R, DakotaError> {
        let url = format!("{}{}", self.base_url, path);
        let is_post = method == Method::POST;

        let resp = observability::client::instrumented("dakota", op, |trace_headers| {
            let mut req = self
                .http
                .request(method.clone(), &url)
                .headers(trace_headers)
                .headers(self.auth_headers(is_post));
            if let Some(b) = body {
                req = req.json(b);
            }
            req.send()
        })
        .await?;

        let status = StatusCode::from_u16(resp.status().as_u16())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let text = resp.text().await?;

        if !status.is_success() {
            return Err(match serde_json::from_str::<ProblemDetails>(&text) {
                Ok(problem) => {
                    warn!(
                        %status,
                        op,
                        detail = problem.detail.as_deref().unwrap_or(""),
                        request_id = problem.request_id.as_deref().unwrap_or(""),
                        "dakota rejected the request"
                    );
                    DakotaError::Api { status, problem: Box::new(problem) }
                }
                Err(_) => DakotaError::Malformed { status, snippet: snippet(&text) },
            });
        }

        // 204 and friends: no body, but the caller may still want `()`.
        if text.trim().is_empty() {
            return serde_json::from_str("null")
                .map_err(|_| DakotaError::Malformed { status, snippet: "empty body".into() });
        }

        serde_json::from_str(&text).map_err(|e| {
            warn!(op, error = %e, "dakota response did not match the expected shape");
            DakotaError::Malformed { status, snippet: snippet(&text) }
        })
    }

    fn auth_headers(&self, with_idempotency: bool) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Ok(v) = HeaderValue::from_str(&self.api_key) {
            h.insert("x-api-key", v);
        }
        if with_idempotency {
            // A fresh key per attempt. Dakota only requires the header to be
            // present and a valid UUID; we are not retrying at this layer, so
            // reusing one across calls would collapse distinct requests.
            if let Ok(v) = HeaderValue::from_str(&Uuid::new_v4().to_string()) {
                h.insert("x-idempotency-key", v);
            }
        }
        h
    }
}

/// Bound an unparseable body so a gateway HTML page cannot flood the logs.
fn snippet(text: &str) -> String {
    const MAX: usize = 300;
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(MAX).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_trailing_slash_is_normalized() {
        let c = DakotaClient::new("https://api.example.com/", "k");
        assert_eq!(c.base_url(), "https://api.example.com");
    }

    #[test]
    fn idempotency_key_only_on_post() {
        let c = DakotaClient::new("https://api.example.com", "k");
        // Dakota 400s a POST without it...
        assert!(c.auth_headers(true).contains_key("x-idempotency-key"));
        // ...and rejects it on every other method.
        assert!(!c.auth_headers(false).contains_key("x-idempotency-key"));
    }

    #[test]
    fn idempotency_keys_are_unique_per_call() {
        let c = DakotaClient::new("https://api.example.com", "k");
        let a = c.auth_headers(true).get("x-idempotency-key").unwrap().clone();
        let b = c.auth_headers(true).get("x-idempotency-key").unwrap().clone();
        assert_ne!(a, b, "a shared key would collapse distinct requests");
    }

    #[test]
    fn api_key_header_is_set() {
        let c = DakotaClient::new("https://api.example.com", "secret-key");
        assert_eq!(
            c.auth_headers(false).get("x-api-key").unwrap().to_str().unwrap(),
            "secret-key"
        );
    }

    #[test]
    fn snippet_is_bounded() {
        let long = "x".repeat(5000);
        let s = snippet(&long);
        assert!(s.chars().count() <= 301, "got {} chars", s.chars().count());
    }

    #[test]
    fn problem_details_parse_from_a_real_dakota_error() {
        // Captured verbatim from the sandbox.
        let raw = r#"{"detail":"amount 5 exceeds sandbox cap of 2; reduce the amount and retry",
            "instance":"/sandbox/simulate/inbound","request_id":"3HNCPFPG5Rt3llmaw7dcacxV8UT",
            "status":400,"title":"Invalid Request",
            "type":"https://docs.dakota.xyz/api-reference/errors#invalid-request"}"#;
        let p: ProblemDetails = serde_json::from_str(raw).unwrap();
        assert_eq!(p.status, Some(400));
        assert!(p.detail.unwrap().contains("sandbox cap"));
        assert_eq!(p.request_id.as_deref(), Some("3HNCPFPG5Rt3llmaw7dcacxV8UT"));
    }

    #[test]
    fn validation_errors_carry_field_detail() {
        let raw = r#"{"title":"Validation Error","status":400,
            "detail":"Request body validation failed - missing required field 'type'",
            "errors":[{"field":"type","message":"missing required field 'type'",
                       "code":"missing_required_field"}]}"#;
        let p: ProblemDetails = serde_json::from_str(raw).unwrap();
        assert_eq!(p.errors.len(), 1);
        assert_eq!(p.errors[0].field.as_deref(), Some("type"));
    }

    #[test]
    fn client_errors_relay_but_server_errors_become_bad_gateway() {
        let problem = Box::new(ProblemDetails {
            r#type: None, title: None, status: Some(400), detail: None,
            instance: None, request_id: None, errors: vec![],
        });
        let e = DakotaError::Api { status: StatusCode::BAD_REQUEST, problem: problem.clone() };
        assert_eq!(e.client_status(), StatusCode::BAD_REQUEST);

        let e = DakotaError::Api { status: StatusCode::INTERNAL_SERVER_ERROR, problem };
        assert_eq!(e.client_status(), StatusCode::BAD_GATEWAY);

        let e = DakotaError::Malformed { status: StatusCode::OK, snippet: "<html>".into() };
        assert_eq!(e.client_status(), StatusCode::BAD_GATEWAY);
    }
}
