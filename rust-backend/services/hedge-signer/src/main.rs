use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use tracing::info;

use hedge_signer::audit::AuditLog;
use hedge_signer::chain::{nonzero_balances, RpcVaultResolver, VaultLookup, VaultResolver};
use hedge_signer::frost::{group_sui_address, Ceremonies, ShareStore};
use hedge_signer::policy::VaultPolicy;
use hedge_signer::state::FrostState;
use hedge_signer::{router, AppState, Cli, Command, Config};
use sui_tx::chain::ChainClient;
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

    // The strict tier pins `vault::return_external` against the deployed
    // trading-vault package (v2: `vault_v2` publish, same module/fn names,
    // token-info key unchanged). token-info is the only
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

    // FROST share store: missing file → empty (no keygen run yet); a
    // present-but-corrupt file is fatal — never boot blind to shares.
    let share_store = ShareStore::open(&cfg.frost_shares_path)
        .with_context(|| format!("opening frost shares {}", cfg.frost_shares_path.display()))?;
    let chain = Arc::new(RpcVaultResolver::new(sui.client.clone(), trading_vault_pkg));

    if let Some(Command::PruneShare { vault_id }) = &cli.command {
        return prune_share(&share_store, chain.as_ref(), &sui.client, vault_id).await;
    }

    let mut vaults: HashMap<String, VaultPolicy> = HashMap::new();
    for vc in &cfg.vaults {
        let policy = VaultPolicy::from_config(vc, trading_vault_pkg)
            .with_context(|| format!("vault {} policy", vc.vault_id))?;
        if vaults.insert(policy.vault_id.clone(), policy).is_some() {
            bail!("duplicate vault_id {} in config", vc.vault_id);
        }
    }

    // Fatal on failure: an unauditable signer must not boot.
    let audit = Arc::new(
        AuditLog::open(&cfg.audit_log_path)
            .with_context(|| format!("opening audit log {}", cfg.audit_log_path.display()))?,
    );

    info!(
        environment = %cfg.environment,
        network = %cfg.network,
        signer = %sui.signer.address,
        trading_vault = %trading_vault_pkg,
        vaults = vaults.len(),
        audit_log = %cfg.audit_log_path.display(),
        frost_shares = %cfg.frost_shares_path.display(),
        "hedge-signer starting"
    );

    let frost_state = Arc::new(FrostState {
        vaults: vaults.clone(),
        audit: audit.clone(),
        ceremonies: Ceremonies::new(share_store),
        chain,
        registrar: Arc::new(sui.signer.keypair.copy()),
    });
    let state = Arc::new(AppState { sui, vaults, audit });
    let proxy = Arc::new(hedge_signer::bluefin_proxy::BluefinProxy::new(
        cfg.bluefin_proxy.clone(),
    ));

    router::serve(cfg.bind_addr, state, frost_state, proxy, &cfg.allowed_origins).await
}

/// `hedge-signer prune-share <vault_id>` — drop an orphaned FROST share.
///
/// Losing a share is unrecoverable, so this refuses on ANY sign of life:
/// the parent address being the vault's registered external account, or
/// holding coins. There is no --force; if both checks pass the share is
/// worthless and the vault can keygen again.
async fn prune_share(
    store: &ShareStore,
    chain: &dyn VaultResolver,
    client: &ChainClient,
    vault_id: &str,
) -> Result<()> {
    let parent = store
        .get(vault_id, |share| {
            group_sui_address(&share.public_key_package)
        })
        .ok_or_else(|| anyhow!("vault {vault_id} has no FROST share"))?
        .context("deriving the stored share's parent address")?;

    match chain
        .resolve(vault_id)
        .await
        .with_context(|| format!("resolving vault {vault_id} on chain"))?
    {
        VaultLookup::NotAVault(why) => bail!("refusing to prune: {why}"),
        VaultLookup::Vault {
            external: Some(account),
        } if account == parent => bail!(
            "refusing to prune: {parent} is vault {vault_id}'s registered external account; \
             deregister it on chain first"
        ),
        VaultLookup::Vault { .. } => {}
    }

    let balances = nonzero_balances(client, parent).await?;
    if !balances.is_empty() {
        bail!(
            "refusing to prune: parent address {parent} still holds {}; \
             sweep it before pruning",
            balances.join(", ")
        );
    }

    store.remove(vault_id)?;
    info!(vault = %vault_id, parent = %parent, "pruned orphaned FROST share");
    Ok(())
}
