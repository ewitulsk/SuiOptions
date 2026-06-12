//! Outbound HTTP instrumentation for the shared client crates
//! (token-info-client, api-service-client, auth-client, indexer-graphql).
//!
//! Wrap each `reqwest` send:
//! ```ignore
//! let resp = observability::client::instrumented("token-info", "GET /tokens", |headers| {
//!     self.http.get(&url).headers(headers).send()
//! })
//! .await?;
//! ```
//! This creates a client span (child of whatever request span is current, so
//! cross-service traces stitch together in Tempo), injects the W3C
//! `traceparent` header, and records `client_request_duration_seconds` /
//! `client_requests_total` labeled by client × op × status.

use std::future::Future;
use std::time::Instant;

use opentelemetry::global;
use opentelemetry_http::HeaderInjector;
use reqwest::header::HeaderMap;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Headers carrying the current span's trace context (W3C `traceparent`).
/// Empty when OTel isn't configured — always safe to attach.
pub fn outbound_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    let cx = tracing::Span::current().context();
    global::get_text_map_propagator(|p| p.inject_context(&cx, &mut HeaderInjector(&mut headers)));
    headers
}

pub async fn instrumented<F, Fut>(
    client: &'static str,
    op: &'static str,
    send: F,
) -> reqwest::Result<reqwest::Response>
where
    F: FnOnce(HeaderMap) -> Fut,
    Fut: Future<Output = reqwest::Result<reqwest::Response>>,
{
    let span = tracing::info_span!(
        "outbound_http",
        otel.name = format!("{client} {op}"),
        otel.kind = "client",
        peer.service = client,
        http.op = op,
        http.status = tracing::field::Empty,
    );
    let headers = {
        let _enter = span.enter();
        outbound_headers()
    };

    let start = Instant::now();
    let result = send(headers).instrument(span.clone()).await;
    let elapsed = start.elapsed().as_secs_f64();

    let status = match &result {
        Ok(resp) => resp.status().as_u16().to_string(),
        Err(_) => "error".to_string(),
    };
    span.record("http.status", status.as_str());
    metrics::counter!(
        "client_requests_total",
        "client" => client, "op" => op, "status" => status.clone(),
    )
    .increment(1);
    metrics::histogram!(
        "client_request_duration_seconds",
        "client" => client, "op" => op,
    )
    .record(elapsed);

    result
}
