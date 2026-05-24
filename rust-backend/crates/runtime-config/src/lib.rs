//! Boot-time runtime configuration plumbing shared by every binary.
//!
//! - [`config_load`] — TOML loader with `${VAR}` env-var expansion.
//! - [`secrets`] — typed loader for per-binary `secrets.toml` (Sui keys,
//!   MM quote key). No env-var fallback by design.
//! - [`logging`] — `tracing_subscriber` setup that scopes `RUST_LOG`
//!   levels to workspace crates only.

pub mod config_load;
pub mod health;
pub mod logging;
pub mod secrets;

pub use secrets::Secrets;
