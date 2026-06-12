//! OpenTelemetry tracer setup: OTLP/HTTP export (Tempo) + W3C propagation.

use opentelemetry::global;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use tracing::Subscriber;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::registry::LookupSpan;

/// Build the OTel layer if `OTEL_EXPORTER_OTLP_ENDPOINT` is set; otherwise
/// tracing runs without span export (local dev). The exporter reads the
/// standard `OTEL_EXPORTER_OTLP_*` env vars itself.
pub(crate) fn init<S>(
    service_name: &'static str,
) -> (
    Option<SdkTracerProvider>,
    Option<OpenTelemetryLayer<S, opentelemetry_sdk::trace::Tracer>>,
)
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_err() {
        return (None, None);
    }

    global::set_text_map_propagator(TraceContextPropagator::new());

    let exporter = match opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .build()
    {
        Ok(e) => e,
        Err(e) => {
            eprintln!("otel exporter init failed, traces disabled: {e}");
            return (None, None);
        }
    };

    let mut resource = Resource::builder().with_service_name(service_name);
    if let Ok(env) = std::env::var("OPTIONS_ENV") {
        resource = resource.with_attribute(KeyValue::new("deployment.environment", env));
    }

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource.build())
        .build();
    let tracer = provider.tracer(service_name);
    global::set_tracer_provider(provider.clone());

    let layer = tracing_opentelemetry::layer().with_tracer(tracer);
    (Some(provider), Some(layer))
}

/// The trace id of the current span, if it belongs to a sampled OTel trace.
/// Useful for stamping responses / WS messages so users can report a trace.
pub fn current_trace_id() -> Option<String> {
    use opentelemetry::trace::TraceContextExt;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let cx = tracing::Span::current().context();
    let span = cx.span();
    let sc = span.span_context();
    sc.is_valid().then(|| sc.trace_id().to_string())
}
