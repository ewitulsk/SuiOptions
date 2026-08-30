//! vault-messenger binary.
//!
//! Boot order follows the house pattern: logging → config (`${VAR}`
//! expansion) → DB pool + embedded migrations → secrets (Sui + EVM relayer
//! keys — hard requirement, the whole point is submitting) → chain clients
//! → watcher + deliverer + crank + alert tasks → axum serve.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use vault_messenger::config::{Config, EvmSecrets};
use vault_messenger::db::{establish_pool, repo::Repo, run_migrations};
use vault_messenger::evm::EvmClient;
use vault_messenger::hub::{HubClient, HubRefs};
use vault_messenger::state::AppState;
use vault_messenger::{alerts, cranks, deliverer, router, watcher, Cli};
use sui_tx::sui_client::SuiClientWrapper;

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("vault-messenger");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let mut cfg =
        Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    // Addresses come from token-info (the served deployments.json record —
    // multichain plan §9: one place to write). TOML values, when set, are
    // break-glass overrides; everything else resolves here or we crash
    // with the list of what's missing.
    let snapshot = token_info_client::TokenInfoClient::new(&cfg.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching snapshot from token-info at {}", cfg.token_info_url))?;
    cfg.resolve_from_token_info(&snapshot)
        .context("resolving config from token-info /package-info")?;
    info!(
        trading_vault_pkg = %cfg.hub.trading_vault_pkg,
        spoke = %cfg.spoke.name,
        spoke_vault = %cfg.spoke.spoke_vault_address,
        spoke_chain_id = cfg.spoke.chain_id,
        "config resolved from token-info"
    );

    let pool = Arc::new(establish_pool(&cfg.database_url, cfg.db_pool_size)?);
    run_migrations(&pool).context("running vault-messenger DB migrations")?;
    let repo = Repo::new(Arc::clone(&pool));
    info!(pool_size = cfg.db_pool_size, "vault-messenger DB ready (migrations applied)");

    // Sui side: client + relayer signer (the address must hold a hub
    // relayer registration — endpoint::add_relayer — and gas).
    let secrets = runtime_config::Secrets::load(&cli.secrets).context("loading secrets")?;
    let sui = SuiClientWrapper::connect(&secrets, cfg.hub.network)
        .await
        .context("connecting sui client")?;

    // EVM side: service key from the same rendered secrets file.
    let evm_secrets = EvmSecrets::load(&cli.secrets).context("loading [evm] secrets")?;
    let evm = EvmClient::new(&cfg.spoke, evm_secrets.private_key()?)
        .context("building EVM client")?;
    info!(
        sui_relayer = %sui.signer.address,
        evm_relayer = %evm.address(),
        "relayer keys loaded — keep both funded with gas"
    );

    let marker = protocol_types::asset::canonicalize_move_type(&cfg.spoke.asset_marker_type);
    let hub = Arc::new(HubClient {
        oracle: oracle_client::OracleClient::new(cfg.hub.oracle_url.trim_end_matches('/')),
        refs: HubRefs::parse(&cfg.hub)?,
        spoke_id: cfg.spoke.spoke_id,
        marker,
        gas_budget: cfg.hub.gas_budget,
        wrap: sui,
    });
    let events = hub.wrap.events.clone();
    let spoke: Arc<dyn vault_messenger::evm::SpokeChain> = Arc::new(evm);

    let state = Arc::new(AppState::new(repo, cfg.spoke.spoke_id as i64));

    watcher::spawn_spoke(watcher::SpokeWatcherParams {
        state: Arc::clone(&state),
        spoke: Arc::clone(&spoke),
        spoke_id: cfg.spoke.spoke_id,
        start_block: cfg.spoke.start_block,
        max_scan_blocks: cfg.spoke.max_scan_blocks.max(1),
        poll_interval: Duration::from_secs(cfg.evm_poll_interval_secs.max(2)),
    });

    watcher::spawn_hub(watcher::HubWatcherParams {
        state: Arc::clone(&state),
        events,
        pkg: cfg.hub.trading_vault_pkg.clone(),
        vault_id: cfg.hub.vault_id.clone(),
        spoke_id: cfg.spoke.spoke_id,
        spoke_app: watcher::pad_evm_address(&cfg.spoke.spoke_vault_address),
        gate_event_types: cfg.hub.config_sync_event_types.clone(),
        poll_interval: Duration::from_secs(cfg.hub_poll_interval_secs.max(2)),
    });

    deliverer::spawn(deliverer::DelivererParams {
        state: Arc::clone(&state),
        hub: hub.clone(),
        spoke: Arc::clone(&spoke),
        submit_to_spoke: cfg.spoke.transport == "dev-relayer",
        max_attempts: cfg.max_attempts.max(1),
        backoff_base_secs: cfg.backoff_base_secs,
        backoff_cap_secs: cfg.backoff_cap_secs,
        deliver_interval: Duration::from_secs(cfg.deliver_interval_secs.max(2)),
    });

    cranks::spawn(cranks::CrankParams {
        state: Arc::clone(&state),
        hub,
        spoke,
        state_sync_interval: Duration::from_secs(cfg.state_sync_interval_secs.max(30)),
        config_sync_interval: Duration::from_secs(cfg.config_sync_interval_secs.max(60)),
    });

    alerts::spawn(alerts::AlertParams {
        state: Arc::clone(&state),
        interval: Duration::from_secs(cfg.alert_interval_secs.max(10)),
        queue_stalled_after_secs: cfg.queue_stalled_after_secs,
        payout_aged_after_secs: cfg.payout_aged_after_secs,
        fee_pot_low_wei: cfg.fee_pot_low_wei(),
    });

    info!(
        environment = %cfg.environment,
        network_set = %cfg.network_set,
        hub_network = %cfg.hub.network,
        spoke_id = cfg.spoke.spoke_id,
        transport = %cfg.spoke.transport,
        "watchers + deliverer + cranks running"
    );

    router::serve(cfg.bind_addr, state, &cfg.allowed_origins).await
}
