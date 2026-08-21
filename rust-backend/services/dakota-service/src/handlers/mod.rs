//! HTTP handlers.
//!
//! Every handler that touches customer data resolves a [`Caller`] from the
//! verified JWT first and scopes off that — never off a path or body field.

pub mod accounts;
pub mod admin;
pub mod catalog;
pub mod customers;
pub mod flows;
pub mod wallets;

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;

use crate::state::AppState;

pub type ApiError = (StatusCode, String);

pub async fn health() -> &'static str {
    "ok"
}

/// Readiness that actually touches Postgres, so a wedged pool fails the health
/// gate instead of reporting green.
pub async fn ready(State(state): State<Arc<AppState>>) -> Result<&'static str, ApiError> {
    state
        .repo
        .ping()
        .map(|_| "ok")
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()))
}

pub fn internal<E: std::fmt::Display>(e: E) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

pub fn bad_request<E: std::fmt::Display>(e: E) -> ApiError {
    (StatusCode::BAD_REQUEST, e.to_string())
}
