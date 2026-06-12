//! One-call observability setup for every backend service (SO-180).
//!
//! `observability::init("service-name")` replaces
//! `runtime_config::logging::init()` and installs three things:
//!
//! 1. **Logs** — the same workspace-aware `EnvFilter`, formatted as JSON when
//!    stdout is not a TTY (containers) so Promtail/Loki can parse structured
//!    fields (`level`, `fields.alert_id`, span `trace_id`, …). Plain text on
//!    a TTY for local dev. Override with `OBS_LOG_FORMAT=json|text`.
//! 2. **Metrics** — a Prometheus recorder behind the `metrics` facade, plus
//!    process metrics (CPU, RSS, FDs). Rendered at `GET /metrics` either via
//!    [`ops::spawn`] (services without an HTTP stack) or
//!    [`middleware::metrics_route`] (axum services, feature `axum`).
//! 3. **Traces** — an OpenTelemetry layer exporting OTLP/HTTP to Tempo when
//!    `OTEL_EXPORTER_OTLP_ENDPOINT` is set (e.g. `http://tempo:4318`). W3C
//!    `traceparent` propagation: inbound via [`middleware::http_obs`],
//!    outbound via [`client::instrumented`].
//!
//! Alerting convention: `error!(alert_id = "some-stable-id", ...)` anywhere
//! in any service fires the provisioned Grafana alert rule, grouped by
//! `alert_id`. No infra changes needed per alert.

use std::io::IsTerminal;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

pub mod client;
pub mod metrics;
#[cfg(feature = "axum")]
pub mod middleware;
pub mod ops;
mod otel;

pub use otel::current_trace_id;

/// Keeps the OTel tracer provider alive; flushes pending spans on drop.
/// Hold it in `main` for the life of the process:
/// `let _obs = observability::init("api-service");`
pub struct ObsGuard {
    tracer_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

impl Drop for ObsGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.tracer_provider.take() {
            if let Err(e) = provider.shutdown() {
                eprintln!("otel tracer shutdown error: {e}");
            }
        }
    }
}

/// Install logging + metrics + tracing. See module docs.
#[must_use]
pub fn init(service_name: &'static str) -> ObsGuard {
    init_with(service_name, &[])
}

/// Like [`init`], but adds extra `EnvFilter` directives (e.g. muting a
/// dependency that's noisy even at `warn`).
#[must_use]
pub fn init_with(service_name: &'static str, extra_directives: &[&str]) -> ObsGuard {
    let filter = runtime_config::logging::build_filter(
        std::env::var("RUST_LOG").ok().as_deref(),
        extra_directives,
    );

    let json = match std::env::var("OBS_LOG_FORMAT").ok().as_deref() {
        Some("json") => true,
        Some("text") => false,
        _ => !std::io::stdout().is_terminal(),
    };
    let fmt_layer = if json {
        // `fields.*` nesting matches the Promtail json pipeline stage; the
        // span list carries `trace_id` recorded on request/message root spans.
        tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(true)
            .with_span_list(true)
            .boxed()
    } else {
        tracing_subscriber::fmt::layer().boxed()
    };

    let (tracer_provider, otel_layer) = otel::init(service_name);

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(otel_layer)
        .init();

    metrics::install(service_name);

    if tracer_provider.is_some() {
        tracing::info!(service = service_name, "otel trace export enabled");
    }

    ObsGuard { tracer_provider }
}
