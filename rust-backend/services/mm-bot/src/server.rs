//! mm-bot's HTTP ops server (SO-348): axum replacement for
//! `observability::ops::spawn`, byte-compatible on `/health` (503
//! "starting" until `Readiness::ready`, then 200 "ok") and `/metrics`
//! (Prometheus text, ungated), plus the read-only desk state API:
//!
//!   GET /desk/state → `{"enabled": false}` when `[desk]` is off,
//!                     503 while an enabled desk is still booting,
//!                     otherwise the full `DeskStateDto` snapshot.
//!
//! CORS is permissive — every deployed read surface in this repo runs
//! with `allowed_origins = ["*"]`, and this port serves nothing
//! mutating.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use observability::ops::Readiness;
use tower_http::cors::{Any, CorsLayer};

use crate::desk::{history, state, Desk};

pub struct ServerParams {
    pub addr: SocketAddr,
    pub readiness: Readiness,
    pub network: String,
    pub desk_enabled: bool,
    /// Filled by `main` once `spawn_desk` returns; empty while booting
    /// (or forever, when the desk is disabled).
    pub desk: Arc<OnceLock<Arc<Desk>>>,
    /// TimescaleDB history handle (SO-349); `None` when no database is
    /// configured.
    pub history: Option<Arc<history::History>>,
}

struct ServerState {
    readiness: Readiness,
    network: String,
    desk_enabled: bool,
    desk: Arc<OnceLock<Arc<Desk>>>,
    history: Option<Arc<history::History>>,
}

/// Spawn the ops server as a background task (same contract as
/// `observability::ops::spawn`).
pub fn spawn(p: ServerParams) {
    tokio::spawn(async move {
        if let Err(e) = serve(p).await {
            tracing::error!(error = %format!("{e:#}"), "ops server exited");
        }
    });
}

async fn serve(p: ServerParams) -> anyhow::Result<()> {
    let addr = p.addr;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "ops server listening (/health, /metrics, /desk/state)");
    axum::serve(listener, app(p)).await?;
    Ok(())
}

fn app(p: ServerParams) -> Router {
    let state = Arc::new(ServerState {
        readiness: p.readiness,
        network: p.network,
        desk_enabled: p.desk_enabled,
        desk: p.desk,
        history: p.history,
    });
    Router::new()
        .route("/health", get(health))
        .route("/desk/state", get(desk_state))
        .route("/desk/history", get(desk_history))
        .with_state(state)
        .merge(observability::middleware::metrics_route())
        .layer(axum::middleware::from_fn(
            observability::middleware::http_obs,
        ))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
}

/// 503 "starting" → 200 "ok", exactly the `observability::ops` contract
/// (`deploy.sh` gates on `curl -fsS`, which fails on 5xx — SO-324).
async fn health(State(s): State<Arc<ServerState>>) -> Response {
    if s.readiness.is_ready() {
        (StatusCode::OK, "ok").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "starting").into_response()
    }
}

#[derive(serde::Serialize)]
struct StateEnvelope {
    enabled: bool,
    /// `None` flattens to nothing (the disabled shape is `{"enabled": false}`).
    #[serde(flatten)]
    state: Option<state::DeskStateDto>,
}

/// Range queries over the TimescaleDB history (SO-349).
async fn desk_history(
    State(s): State<Arc<ServerState>>,
    axum::extract::Query(q): axum::extract::Query<history::HistoryQuery>,
) -> Response {
    if !s.desk_enabled {
        return Json(StateEnvelope { enabled: false, state: None }).into_response();
    }
    let Some(history) = s.history.clone() else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "enabled": true, "error": "history not configured" })),
        )
            .into_response();
    };
    if !history.is_ready() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "enabled": true, "error": "history db not ready" })),
        )
            .into_response();
    }
    match tokio::task::spawn_blocking(move || history.query(&q)).await {
        Ok(Ok(resp)) => Json(resp).into_response(),
        Ok(Err(e)) => {
            tracing::warn!(error = %format!("{e:#}"), "desk history query failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("{e:#}") })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn desk_state(State(s): State<Arc<ServerState>>) -> Response {
    if !s.desk_enabled {
        return Json(StateEnvelope { enabled: false, state: None }).into_response();
    }
    match s.desk.get() {
        Some(desk) => {
            let dto = state::snapshot(desk, &s.network).await;
            Json(StateEnvelope { enabled: true, state: Some(dto) }).into_response()
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "enabled": true, "error": "desk starting" })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn spawn_test_server(desk_enabled: bool) -> (SocketAddr, Readiness) {
        let readiness = Readiness::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = app(ServerParams {
            addr,
            readiness: readiness.clone(),
            network: "testnet".into(),
            desk_enabled,
            desk: Arc::new(OnceLock::new()),
            history: None,
        });
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (addr, readiness)
    }

    #[tokio::test]
    async fn health_is_503_until_ready_then_200_and_metrics_ungated() {
        let (addr, readiness) = spawn_test_server(false).await;
        let client = reqwest::Client::new();
        let r = client.get(format!("http://{addr}/health")).send().await.unwrap();
        assert_eq!(r.status(), 503);
        assert_eq!(r.text().await.unwrap(), "starting");
        // /metrics serves while not ready.
        let r = client.get(format!("http://{addr}/metrics")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        readiness.ready();
        let r = client.get(format!("http://{addr}/health")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.text().await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn desk_state_reports_disabled_and_starting() {
        // Disabled desk: 200 with enabled=false (the prod shape).
        let (addr, _readiness) = spawn_test_server(false).await;
        let client = reqwest::Client::new();
        let r = client.get(format!("http://{addr}/desk/state")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v, serde_json::json!({ "enabled": false }));

        // Enabled but not yet booted: 503 so pollers retry.
        let (addr, _readiness) = spawn_test_server(true).await;
        let r = client.get(format!("http://{addr}/desk/state")).send().await.unwrap();
        assert_eq!(r.status(), 503);
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["enabled"], true);
    }

    #[tokio::test]
    async fn desk_history_404s_without_a_configured_db() {
        let (addr, _readiness) = spawn_test_server(true).await;
        let client = reqwest::Client::new();
        let r = client
            .get(format!("http://{addr}/desk/history?series=snapshots"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["error"], "history not configured");
    }
}
