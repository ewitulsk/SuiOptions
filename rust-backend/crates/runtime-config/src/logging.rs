//! Workspace-aware tracing setup.
//!
//! `RUST_LOG=debug` (or `trace`, `info`, etc.) applies the level only to
//! our workspace crates; every other crate stays at `warn`. So debug logs
//! from `sui_sdk`, `tokio`, `hyper`, `diesel`, and friends stay out of the
//! output by default.
//!
//! Anything more elaborate in `RUST_LOG` (a value containing `,` or `=`)
//! is passed straight to `EnvFilter` as a directive list — the escape
//! hatch for "I really do want to crank `sui_sdk` to trace this once."

use tracing_subscriber::EnvFilter;

/// Workspace crates whose logs are considered "ours". Names are Rust crate
/// names (hyphens become underscores), so `quoting-service` is
/// `quoting_service` here.
const OUR_CRATES: &[&str] = &[
    "protocol_types",
    "runtime_config",
    "cli_spec",
    "pyth_client",
    "sui_tx",
    "pricing",
    "indexer",
    "quoting_service",
    "mm_bot",
    "option_scheduler",
    "api_service",
    "token_info",
    "token_info_client",
    "deployment_manager",
    "exchange",
    "writer",
    "rfq_monitor",
    "control_panel",
    "integration_tests",
];

/// Install the global tracing subscriber. See module docs for `RUST_LOG`
/// semantics.
pub fn init() {
    init_with(&[]);
}

/// Like [`init`], but adds extra directives on top — useful for muting a
/// specific dependency that's noisy even at `warn`.
pub fn init_with(extra_directives: &[&str]) {
    let filter = build_filter(std::env::var("RUST_LOG").ok().as_deref(), extra_directives);
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn build_filter(rust_log: Option<&str>, extras: &[&str]) -> EnvFilter {
    // Power-user form: a directive list. Hand it to EnvFilter verbatim.
    if let Some(v) = rust_log {
        if v.contains('=') || v.contains(',') {
            let mut f = EnvFilter::new(v);
            for d in extras {
                f = f.add_directive(d.parse().expect("valid tracing directive"));
            }
            return f;
        }
    }

    let level = rust_log.unwrap_or("info");
    let mut f = EnvFilter::new("warn");
    for c in OUR_CRATES {
        f = f.add_directive(
            format!("{c}={level}")
                .parse()
                .expect("valid tracing directive"),
        );
    }
    for d in extras {
        f = f.add_directive(d.parse().expect("valid tracing directive"));
    }
    f
}
