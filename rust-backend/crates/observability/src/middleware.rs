//! axum layer: per-request trace span + HTTP RED metrics + `/metrics` route.
//!
//! Apply to every axum router:
//! ```ignore
//! Router::new()
//!     .merge(observability::middleware::metrics_route())
//!     .layer(axum::middleware::from_fn(observability::middleware::http_obs))
//! ```
//!
//! Each request gets a span continuing the caller's W3C `traceparent` (or
//! starting a new trace), with `trace_id` recorded so it lands in JSON logs
//! and is echoed back in the `x-trace-id` response header. Metrics:
//! `http_requests_total` / `http_request_duration_seconds` /
//! `http_requests_in_flight`, labeled by method × route × status (route is
//! the matched pattern, not the raw path, to bound cardinality).

use std::time::Instant;

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use opentelemetry::global;
use opentelemetry_http::HeaderExtractor;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// `GET /metrics` in Prometheus text format. Mount on the service router —
/// service ports are only reachable inside the docker network / VPC and
/// nginx routes no `/metrics` path publicly.
pub fn metrics_route<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new().route("/metrics", get(|| async { crate::metrics::render() }))
}

pub async fn http_obs(req: Request, next: Next) -> Response {
    let method = req.method().as_str().to_owned();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".to_owned());

    // /health and /metrics are scrape noise — pass through untraced.
    if route == "/health" || route == "/metrics" {
        return next.run(req).await;
    }

    let parent = global::get_text_map_propagator(|p| p.extract(&HeaderExtractor(req.headers())));
    let span = tracing::info_span!(
        "http_request",
        otel.name = format!("{method} {route}"),
        http.method = %method,
        http.route = %route,
        http.status = tracing::field::Empty,
        trace_id = tracing::field::Empty,
    );
    span.set_parent(parent);
    let trace_id = {
        let _enter = span.enter();
        crate::current_trace_id()
    };
    if let Some(id) = &trace_id {
        span.record("trace_id", id.as_str());
    }

    let labels = [("method", method.clone()), ("route", route.clone())];
    metrics::gauge!("http_requests_in_flight", &labels).increment(1.0);
    let start = Instant::now();

    let mut resp = next.run(req).instrument(span.clone()).await;

    let elapsed = start.elapsed().as_secs_f64();
    metrics::gauge!("http_requests_in_flight", &labels).decrement(1.0);
    let status = resp.status().as_u16().to_string();
    span.record("http.status", &status);
    metrics::counter!(
        "http_requests_total",
        "method" => method.clone(),
        "route" => route.clone(),
        "status" => status.clone(),
    )
    .increment(1);
    metrics::histogram!(
        "http_request_duration_seconds",
        "method" => method,
        "route" => route,
        "status" => status,
    )
    .record(elapsed);

    if let Some(id) = trace_id {
        if let Ok(v) = id.parse() {
            resp.headers_mut().insert("x-trace-id", v);
        }
    }
    resp
}

/// Convenience: a 500 response helper services can use in error paths so the
/// middleware sees a real status (axum handlers usually do this already).
pub fn internal_error(msg: impl Into<String>) -> Response {
    (axum::http::StatusCode::INTERNAL_SERVER_ERROR, msg.into()).into_response()
}
