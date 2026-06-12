//! Prometheus recorder behind the `metrics` facade + process metrics.

use std::sync::OnceLock;
use std::time::Duration;

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Seconds buckets for `*_duration_seconds` histograms: 1ms → 10s.
const DURATION_BUCKETS: &[f64] = &[
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Install the global recorder and spawn the process-metrics / upkeep loop.
/// Idempotent; later calls are no-ops (useful in tests).
pub(crate) fn install(service_name: &'static str) {
    let recorder = PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Suffix("duration_seconds".to_string()),
            DURATION_BUCKETS,
        )
        .expect("non-empty histogram buckets")
        .install_recorder();
    let handle = match recorder {
        Ok(h) => h,
        Err(e) => {
            // A recorder is already installed (e.g. double init in tests).
            tracing::warn!(error = %e, "metrics recorder already installed");
            return;
        }
    };
    let _ = HANDLE.set(handle.clone());

    metrics::gauge!("service_info", "service" => service_name).set(1.0);

    tokio::spawn(async move {
        #[cfg(target_os = "linux")]
        let collector = {
            let c = metrics_process::Collector::default();
            c.describe();
            c
        };
        loop {
            #[cfg(target_os = "linux")]
            collector.collect();
            handle.run_upkeep();
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    });
}

/// Render the current metrics in Prometheus text exposition format.
pub fn render() -> String {
    HANDLE.get().map(|h| h.render()).unwrap_or_default()
}
