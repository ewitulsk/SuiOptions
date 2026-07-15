//! cctp-relay binary.
//!
//! Boot order follows the house pattern: logging → config (`${VAR}`
//! expansion) → DB pool + embedded migrations → secrets (relayer keys —
//! hard requirement, the whole point is auto-minting) → Sui client →
//! watcher + relayer tasks → axum serve.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use cctp_relay::config::Config;
use cctp_relay::db::{establish_pool, repo::Repo, run_migrations};
use cctp_relay::iris::IrisClient;
use cctp_relay::solana_mint::{parse_keypair, SolanaMinter};
use cctp_relay::solana_rpc::SolanaRpc;
use cctp_relay::state::AppState;
use cctp_relay::sui_mint::SuiMinter;
use cctp_relay::{relayer, router, watcher, Cli};
use sui_tx::sui_client::SuiClientWrapper;

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("cctp-relay");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    let pool = Arc::new(establish_pool(&cfg.database_url, cfg.db_pool_size)?);
    run_migrations(&pool).context("running cctp-relay DB migrations")?;
    let repo = Repo::new(Arc::clone(&pool));
    info!(pool_size = cfg.db_pool_size, "cctp-relay DB ready (migrations applied)");

    let secrets = runtime_config::Secrets::load(&cli.secrets).context("loading secrets")?;

    // Sui side: client + relayer signer.
    let sui = SuiClientWrapper::connect(&secrets, cfg.sui.network)
        .await
        .context("connecting sui client")?;

    // Solana side: fee-payer keypair + thin JSON-RPC client.
    let solana_key = secrets
        .solana_private_key(&cfg.solana.network)
        .context("resolving solana relayer key")?;
    let solana_keypair = parse_keypair(solana_key)?;
    let solana_rpc = SolanaRpc::new(&cfg.solana.rpc_url);
    let solana_minter = SolanaMinter {
        rpc: solana_rpc.clone(),
        keypair: solana_keypair,
        usdc_mint: cfg.solana.usdc_mint.parse().context("bad solana usdc_mint")?,
    };
    info!(
        sui_relayer = %sui.signer.address,
        solana_relayer = %solana_minter.address(),
        "relayer keys loaded — keep both funded with gas"
    );

    let state = Arc::new(AppState::new(repo));

    watcher::spawn(watcher::WatcherParams {
        state: Arc::clone(&state),
        iris: IrisClient::new(&cfg.iris_base_url),
        sui: sui.client.clone(),
        solana: solana_rpc,
        poll_interval: Duration::from_secs(cfg.poll_interval_secs.max(2)),
    });

    relayer::spawn(relayer::RelayerParams {
        state: Arc::clone(&state),
        sui: SuiMinter { client: sui.client, signer: sui.signer, cfg: cfg.sui.clone() },
        solana: solana_minter,
        relay_interval: Duration::from_secs(cfg.relay_interval_secs.max(2)),
        max_mint_attempts: cfg.max_mint_attempts.max(1),
    });

    info!(
        environment = %cfg.environment,
        iris = %cfg.iris_base_url,
        sui_network = %cfg.sui.network,
        solana_network = %cfg.solana.network,
        "watcher + relayer running"
    );

    router::serve(cfg.bind_addr, state, &cfg.allowed_origins).await
}
