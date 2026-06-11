//! Library surface for the `deploy` binary (the deployment manager).
//!
//! Owns every submodule plus the clap [`Cli`] type so the control-panel TUI
//! can introspect the binary's flags. The actual deploy pipeline lives in
//! `main.rs`.

pub mod deploy;
pub mod json_store;
pub mod network;
pub mod signer;

use std::path::PathBuf;

use clap::Parser;

use crate::network::Network;

#[derive(Parser, Debug)]
#[command(
    name = "deploy",
    version,
    about = "Deploy options-protocol contracts to Sui networks and record addresses."
)]
pub struct Cli {
    /// Deployment environment to record this under in deployments.json
    /// (`dev` / `staging` / `prod`). This is the slot key; the Sui network
    /// the package actually publishes to is `--network`. Two envs can
    /// target the same network as distinct deployments.
    #[arg(short = 'e', long)]
    pub env: String,

    /// Sui network to publish to (also picks the RPC + the signing key
    /// slot in secrets.toml).
    #[arg(short, long, value_enum)]
    pub network: Network,

    /// Path to the Move package containing the contracts.
    /// Default assumes the manager is run from `rust-backend/`.
    #[arg(short, long, default_value = "../contracts")]
    pub contracts: PathBuf,

    /// Path to the JSON file that tracks deployments per network.
    #[arg(short, long, default_value = "deployments.json")]
    pub output: PathBuf,

    /// Per-binary secrets TOML. Holds the Sui signing key. There is no
    /// env-var fallback — if the file is missing or the key for the
    /// targeted network is absent, deploy refuses to start.
    #[arg(short = 's', long, default_value = "tools/deployment-manager/config/secrets.toml")]
    pub secrets: PathBuf,

    /// Gas budget (MIST) per transaction.
    #[arg(long, default_value_t = 500_000_000)]
    pub gas_budget: u64,

    /// Skip the post-publish `treasury::create_and_share` call. Use when
    /// re-publishing for testing and you don't need a fresh Treasury.
    #[arg(long)]
    pub skip_init: bool,

    /// Also publish the test-tokens package (TUSDC/TBTC/TWAL/TDEEP) and
    /// record the faucet IDs in deployments.json. Each run publishes a
    /// fresh package and overwrites the previous testTokens block.
    #[arg(long)]
    pub deploy_tokens: bool,

    /// Path to the test-tokens Move package.
    #[arg(long, default_value = "../test-tokens")]
    pub test_tokens: PathBuf,

    /// Also publish the siws_session (session-tokens) package the protocol
    /// depends on, BEFORE the protocol publish. Rewrites the session
    /// package's Move.toml address to the fresh id so the protocol links
    /// against it, and records the package + Registry ids in
    /// deployments.json. Required the first time a protocol build that
    /// depends on a changed session package is deployed.
    #[arg(long)]
    pub deploy_session: bool,

    /// Path to the siws_session Move package.
    #[arg(long, default_value = "../session-tokens/contracts")]
    pub session_contracts: PathBuf,
}

cli_spec::define_program! {
    id          = "deploy",
    cargo_pkg   = "deployment-manager",
    working_dir = ".",
    description = "Compiles and publishes the options-protocol Move package and records every \
                   important on-chain id into deployments.json. Optionally also publishes the \
                   test-tokens package.",
    cli         = crate::Cli,
}
