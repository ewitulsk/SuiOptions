//! Library surface for the `deploy` binary (the deployment manager).
//!
//! Owns every submodule plus the clap [`Cli`] type so the control-panel TUI
//! can introspect the binary's flags. The actual deploy pipeline lives in
//! `main.rs`.

pub mod deploy;
pub mod evm_spoke;
pub mod exchange_markets;
pub mod json_store;
pub mod network;
pub mod signer;
pub mod trading_vault_init;

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

    /// Override the gRPC endpoint used to publish. Defaults to the shared
    /// `[sui] grpc_url` from the secrets file, else the network's public
    /// fullnode (picked by `--network`). Pass a private/dedicated endpoint
    /// here when the public one is rate-limiting or timing out.
    ///
    /// `--rpc` is accepted as a deprecated alias so existing CI invocations
    /// keep working; JSON-RPC itself is gone (docs/sui-json-rpc-migration.md).
    #[arg(long, alias = "rpc")]
    pub grpc: Option<String>,

    /// Path to the contracts tree root containing the four Move packages
    /// (`core/`, `auction/`, `rfq/`, `vault/`), published in dependency
    /// order. Default assumes the manager is run from `rust-backend/`.
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

    /// Publish ONLY the cctp_bridge package (cctp-contracts/) and record it
    /// under the env's `cctpBridge` block. Skips the protocol publish
    /// entirely; the env must already exist in deployments.json.
    #[arg(long)]
    pub deploy_cctp: bool,

    /// Publish ONLY the hybrid-exchange settlement package
    /// (contracts/exchange/), create its markets, and record both under
    /// the env's `exchange` block. Skips the protocol publish entirely;
    /// the env must already exist in deployments.json.
    ///
    /// The default protocol publish ALSO republishes the exchange every
    /// redeploy (a testnet redeploy invalidates open orders by
    /// definition); this flag exists for exchange-only iteration.
    #[arg(long)]
    pub deploy_exchange: bool,

    /// Path to the cctp-contracts Move package.
    #[arg(long, default_value = "../cctp-contracts")]
    pub cctp_contracts: PathBuf,

    /// Publish ONLY the mm_collateral template (contracts/mm-collateral) and
    /// write the state file mm-bot serves from. Sign with the MM-BOT key
    /// (`--secrets`), NOT the deployer — the created CollateralAccount is
    /// owned by the publisher. Must re-run after every options_core republish
    /// (the template deps on core by local path); the redeploy-contract
    /// workflow does this right after the protocol publish.
    #[arg(long)]
    pub deploy_mm_collateral: bool,

    /// Path to the mm-collateral Move package template.
    #[arg(long, default_value = "../contracts/mm-collateral")]
    pub mm_collateral_contracts: PathBuf,

    /// Where `--deploy-mm-collateral` writes the state file. Defaults to
    /// `services/mm-bot/config/collateral.<network>.toml` — the committed
    /// path the deploy bundle ships to the host for the mm-bot mount.
    #[arg(long)]
    pub collateral_out: Option<PathBuf>,

    /// Gas budget (MIST) per transaction.
    #[arg(long, default_value_t = 500_000_000)]
    pub gas_budget: u64,

    /// Skip the post-publish `treasury::create_and_share` call. Use when
    /// re-publishing for testing and you don't need a fresh Treasury.
    #[arg(long)]
    pub skip_init: bool,

    /// Also publish the test-tokens package (TUSDC/TBTC/TWAL/TSUI) and
    /// record the faucet IDs in deployments.json. Each run publishes a
    /// fresh package and overwrites the previous testTokens block.
    #[arg(long)]
    pub deploy_tokens: bool,

    /// Path to the test-tokens Move package.
    #[arg(long, default_value = "../test-tokens")]
    pub test_tokens: PathBuf,

    /// Also publish the two Sui transport packages
    /// (contracts/endpoint-layerzero + contracts/endpoint-ccip) after the
    /// protocol tree and record them under `endpointLayerzero` /
    /// `endpointCcip`. They link against the fresh core + vault_v2 by
    /// local path, so — like the protocol packages — a core republish
    /// orphans the previous transports; redeploy them together. Without
    /// this flag the previous blocks are carried forward untouched
    /// (exactly like cctpBridge).
    #[arg(long)]
    pub deploy_endpoints: bool,

    /// Protocol chain id of the hub (envelope namespace,
    /// multichain-vault-plan §2.1), recorded as `multichain.hubChainId`
    /// alongside the EndpointRegistry the trading-vault-v2 publish
    /// creates.
    #[arg(long, default_value_t = 1)]
    pub hub_chain_id: u64,

    /// Merge a forge deploy artifact (written by
    /// evm-contracts/script/DeploySpoke.s.sol) into the env's
    /// `multichain.spokes.<name>` block. Standalone pass: no publish, no
    /// chain access — read-merge-write on deployments.json only. The env
    /// must already exist with a `multichain` block (deploy the protocol
    /// first).
    #[arg(long, value_name = "ARTIFACT_JSON")]
    pub record_evm_spoke: Option<PathBuf>,

    /// Extra address to seed into the shared ingress Whitelist during the
    /// ceremony. `addr` seeds ALL four domains; `addr=options,vault-lp`
    /// seeds only those domains (options, exchange, vault-create,
    /// vault-lp). Repeatable; merged (domain sets unioned) with the env's
    /// baked-in INGRESS_MEMBERS list. The deployer is always seeded into
    /// every domain automatically.
    #[arg(long = "ingress-member")]
    pub ingress_member: Vec<String>,
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
