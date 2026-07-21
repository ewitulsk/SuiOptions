use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use tracing::info;

use hedge_signer::audit::AuditLog;
use hedge_signer::policy::VaultPolicy;
use hedge_signer::{router, AppState, Cli, Config};
use sui_tx::SuiClientWrapper;
use token_info_client::TokenInfoClient;

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("hedge-signer");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;

    let sui = SuiClientWrapper::connect(&secrets, cfg.network)
        .await
        .context("connecting Sui client + hedge-signer key")?;

    // The strict tier pins `trading_vault::vault::return_external` against
    // the deployed trading_vault package. token-info is the only
    // deployments.json reader — a deployment without the package can't
    // classify sweeps, so fail at boot.
    let snapshot = TokenInfoClient::new(&cfg.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching token-info from {}", cfg.token_info_url))?;
    let trading_vault_pkg = snapshot
        .trading_vault()
        .context("trading_vault package missing from token-info package_info")?
        .package()
        .context("trading_vault package id from token-info")?;

    let mut vaults: HashMap<String, VaultPolicy> = HashMap::new();
    for vc in &cfg.vaults {
        let policy = VaultPolicy::from_config(vc, trading_vault_pkg)
            .with_context(|| format!("vault {} policy", vc.vault_id))?;
        if vaults.insert(policy.vault_id.clone(), policy).is_some() {
            bail!("duplicate vault_id {} in config", vc.vault_id);
        }
    }

    // Fatal on failure: an unauditable signer must not boot.
    let audit = AuditLog::open(&cfg.audit_log_path)
        .with_context(|| format!("opening audit log {}", cfg.audit_log_path.display()))?;

    info!(
        environment = %cfg.environment,
        network = %cfg.network,
        signer = %sui.signer.address,
        trading_vault = %trading_vault_pkg,
        vaults = vaults.len(),
        audit_log = %cfg.audit_log_path.display(),
        "hedge-signer starting"
    );

    let state = Arc::new(AppState { sui, vaults, audit });

    router::serve(cfg.bind_addr, state, &cfg.allowed_origins).await
}
