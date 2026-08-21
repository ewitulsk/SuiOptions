// Package obs wires the shared observability surface every service mounts:
// GET /health returning the literal body "ok" (gatus asserts [BODY] == ok),
// GET /metrics via promhttp, and OTLP trace export when
// OTEL_EXPORTER_OTLP_ENDPOINT is set (fully no-op when unset — local dev
// never pays the tracing tax).
package obs

import (
	"context"
	"net/http"
	"os"
	"time"

	"github.com/prometheus/client_golang/prometheus/promhttp"
	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/exporters/otlp/otlptrace/otlptracehttp"
	"go.opentelemetry.io/otel/sdk/resource"
	sdktrace "go.opentelemetry.io/otel/sdk/trace"
	semconv "go.opentelemetry.io/otel/semconv/v1.26.0"
	"go.opentelemetry.io/otel/trace"
	nooptrace "go.opentelemetry.io/otel/trace/noop"
)

// MountHealthAndMetrics registers GET /health and GET /metrics on mux.
func MountHealthAndMetrics(mux *http.ServeMux) {
	mux.HandleFunc("GET /health", func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("content-type", "text/plain; charset=utf-8")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte("ok"))
	})
	mux.Handle("GET /metrics", promhttp.Handler())
}

// InitTracing configures the global tracer provider from
// OTEL_EXPORTER_OTLP_ENDPOINT. Unset endpoint → a no-op provider, matching
// how the Rust services gate on the same variable. The returned func flushes
// and shuts the provider down; call it on graceful exit.
func InitTracing(ctx context.Context, serviceName string) func() {
	endpoint := os.Getenv("OTEL_EXPORTER_OTLP_ENDPOINT")
	if endpoint == "" {
		otel.SetTracerProvider(nooptrace.NewTracerProvider())
		return func() {}
	}

	exporter, err := otlptracehttp.New(ctx,
		otlptracehttp.WithEndpointURL(endpoint),
		otlptracehttp.WithInsecure(),
	)
	if err != nil {
		// Bad endpoint config must not take the service down; fall back to
		// no-op like the unset case.
		otel.SetTracerProvider(nooptrace.NewTracerProvider())
		return func() {}
	}

	tp := sdktrace.NewTracerProvider(
		sdktrace.WithBatcher(exporter),
		sdktrace.WithResource(resource.NewWithAttributes(
			semconv.SchemaURL,
			semconv.ServiceName(serviceName),
		)),
	)
	otel.SetTracerProvider(tp)
	return func() {
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = tp.Shutdown(shutdownCtx)
	}
}

// Tracer is the per-package tracer helper.
func Tracer(name string) trace.Tracer {
	return otel.Tracer(name)
}
