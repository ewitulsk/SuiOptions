//! The signer node service (bridge-spec.md §5). Wraps `bridge-signer` behind
//! the §5.3 HTTP surface and enforces the §5.4 source-commitment boundary
//! before signing.

use std::path::PathBuf;

use clap::Parser;

pub mod config;
pub mod handlers;
pub mod router;
pub mod state;
pub mod verifier;

pub use config::Config;
pub use state::AppState;

#[derive(Debug, Parser)]
#[command(name = "bridge-signer-service", about = "Layer 1 signer node (M1, single-party)")]
pub struct Cli {
    #[arg(long, default_value = "config.toml")]
    pub config: PathBuf,
}
