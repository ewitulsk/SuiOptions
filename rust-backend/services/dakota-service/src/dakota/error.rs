//! Dakota returns RFC 9457 Problem Details on every failure. Preserving the
//! status and `detail` verbatim matters: Dakota's messages are specific and
//! actionable ("capabilities are required", "Customer is not KYB-approved by
//! Dakota", "amount 5 exceeds sandbox cap of 2"), and flattening them into a
//! generic 502 would throw away the one thing that tells an operator what to fix.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

/// RFC 9457 Problem Details, as Dakota emits it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProblemDetails {
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub instance: Option<String>,
    /// Dakota's correlation id. Always worth surfacing — it is what their
    /// support asks for first.
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub errors: Vec<FieldError>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FieldError {
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum DakotaError {
    /// Dakota answered, and said no.
    #[error("dakota {status}: {}", .problem.detail.as_deref().unwrap_or("(no detail)"))]
    Api {
        status: StatusCode,
        problem: Box<ProblemDetails>,
    },
    /// Dakota answered with something we could not parse — a gateway error
    /// page, a truncated body, a schema change.
    #[error("dakota returned an unreadable {status} body: {snippet}")]
    Malformed { status: StatusCode, snippet: String },
    /// We never got an answer.
    #[error("reaching dakota: {0}")]
    Transport(#[from] reqwest::Error),
}

impl DakotaError {
    /// Status to hand back to our own caller.
    ///
    /// A 4xx from Dakota is relayed unchanged, because it is genuinely the
    /// caller's problem to fix. Everything else becomes 502: it is our
    /// dependency that failed, not their request.
    pub fn client_status(&self) -> StatusCode {
        match self {
            DakotaError::Api { status, .. } if status.is_client_error() => *status,
            _ => StatusCode::BAD_GATEWAY,
        }
    }

    pub fn request_id(&self) -> Option<&str> {
        match self {
            DakotaError::Api { problem, .. } => problem.request_id.as_deref(),
            _ => None,
        }
    }
}

impl IntoResponse for DakotaError {
    fn into_response(self) -> Response {
        let status = self.client_status();
        let body = match &self {
            DakotaError::Api { problem, .. } => serde_json::json!({
                "error": problem.detail.clone().or_else(|| problem.title.clone()),
                "dakota_request_id": problem.request_id,
                "fields": problem.errors,
            }),
            other => serde_json::json!({ "error": other.to_string() }),
        };
        (status, axum::Json(body)).into_response()
    }
}
