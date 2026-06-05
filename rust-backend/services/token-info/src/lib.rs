//! token-info service.
//!
//! The single source of truth for the supported-token catalog and the
//! protocol's on-chain `package_info`. The ONLY service that reads
//! `deployments.json`; every other service and the frontend reads from here
//! (via `token-info-client` / the public HTTP API).
//!
//! Two routers on two ports: a public read API (proxied by nginx) and an
//! internal mutate API (network-isolated). On dev/staging the catalog
//! auto-seeds from `deployments.json` testTokens.

pub mod config;
pub mod db;
pub mod handlers;
pub mod overlay;
pub mod router;
pub mod state;

pub use config::Config;
pub use state::AppState;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "token-info",
    about = "Token catalog + protocol package-info service. Sole reader of deployments.json."
)]
pub struct Cli {
    #[arg(short, long, default_value = "services/token-info/config/config.toml")]
    pub config: PathBuf,
}

cli_spec::define_program! {
    id          = "token-info",
    cargo_pkg   = "token-info",
    working_dir = ".",
    description = "Token catalog + protocol package-info service. The sole reader of \
                   deployments.json; serves a public read API and an internal mutate API.",
    cli         = crate::Cli,
}
