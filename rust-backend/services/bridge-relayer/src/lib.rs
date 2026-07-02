//! Untrusted relayer (bridge-spec.md §2, Layer 3): watch a source Outbox →
//! request an aggregated signature from the signer node → submit the
//! self-verifying message to the destination Inbox.

use std::path::PathBuf;

use clap::Parser;

pub mod config;
pub mod event;
pub mod evm_submit;
pub mod relay;
pub mod signer_client;
pub mod submit;
pub mod sui_source;

pub use config::Config;

#[derive(Debug, Parser)]
#[command(name = "bridge-relayer", about = "Layer 1 message relayer (M1)")]
pub struct Cli {
    #[arg(long, default_value = "config.toml")]
    pub config: PathBuf,
}
