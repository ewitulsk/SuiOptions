//! Library surface for the `solana-deploy` binary — the Solana counterpart
//! of `tools/deployment-manager`. Owns the clap [`Cli`] plus the pure
//! planning / JSON-store modules; the deploy pipeline lives in `main.rs`.

pub mod anchor_toml;
pub mod json_store;
pub mod plan;
pub mod tokens;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use solana_tx::Network;

#[derive(Parser, Debug)]
#[command(
    name = "solana-deploy",
    version,
    about = "Initialize the options programs on a Solana cluster and record \
             every important on-chain id into solana-deployments.json. \
             Program binary deploys stay with the anchor/solana CLI."
)]
pub struct Cli {
    /// Deployment environment to record this under in
    /// solana-deployments.json (`dev` / `staging` / `prod`). This is the
    /// slot key; the Solana cluster is `--network` — two envs can target
    /// the same cluster as distinct deployments.
    #[arg(short = 'e', long, global = true)]
    pub env: Option<String>,

    /// Solana cluster (also picks the keypair slot in secrets.toml).
    /// Required unless running `show`.
    #[arg(short = 'n', long, value_enum)]
    pub network: Option<Network>,

    /// Override the JSON-RPC URL. Defaults to the `solana.rpc_url` secret
    /// when set, else the cluster's public endpoint.
    #[arg(long)]
    pub rpc: Option<String>,

    /// Path to the JSON file that tracks deployments per env. The default
    /// assumes the tool is run from `rust-backend/`.
    #[arg(short = 'o', long, default_value = "solana-deployments.json", global = true)]
    pub output: PathBuf,

    /// Per-binary secrets TOML holding the `[solana]` admin/deployer
    /// keypair. No env-var fallback — missing file or key refuses to start.
    #[arg(
        short = 's',
        long,
        default_value = "tools/solana-deployment-manager/config/secrets.toml"
    )]
    pub secrets: PathBuf,

    /// Path to the solana-contracts workspace; its Anchor.toml supplies
    /// the default program ids.
    #[arg(long, default_value = "../solana-contracts")]
    pub contracts: PathBuf,

    /// Override the options_core program id (default: Anchor.toml).
    #[arg(long)]
    pub core_program_id: Option<String>,

    /// Override the auction_venue program id (default: Anchor.toml).
    #[arg(long)]
    pub venue_program_id: Option<String>,

    /// Override the options_vault program id (default: Anchor.toml).
    #[arg(long)]
    pub vault_program_id: Option<String>,

    /// Don't call `initialize` (record ids only). The existing
    /// initializeSignature is carried forward.
    #[arg(long)]
    pub skip_init: bool,

    /// Create the test SPL mints (TUSDC/TBTC/TSOL) and mint a seed supply
    /// to the deployer. Idempotent: mints already recorded for this env
    /// that still exist on-chain with the right decimals are kept.
    /// Refused on mainnet-beta.
    #[arg(long)]
    pub deploy_tokens: bool,

    /// Final mint authority for the test mints (the solana-gas-station
    /// faucet key). Falls back to the deployer when unset.
    #[arg(long)]
    pub faucet_authority: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Print the current slot for `--env` (or the whole file) and exit.
    Show,
}
