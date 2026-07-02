use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{error, info};

use bridge_relayer::evm_source::EvmSourceWatcher;
use bridge_relayer::evm_submit::EvmDestSubmitter;
use bridge_relayer::relay::{relay_once, Router, SourceWatcher};
use bridge_relayer::signer_client::HttpSigner;
use bridge_relayer::sui_dest::SuiDestSubmitter;
use bridge_relayer::sui_source::SuiSourceWatcher;
use bridge_relayer::{Cli, Config};

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("bridge-relayer");

    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)
        .with_context(|| format!("loading config from {}", cli.config.display()))?;
    let domain_sep = cfg.domain_sep().context("deriving domain separator")?;

    // --- destination router (which direction goes where) ---
    let mut router = Router::default();
    match (&cfg.evm_rpc_url, &cfg.evm_inbox_addr, &cfg.evm_relayer_key) {
        (Some(rpc), Some(inbox), Some(key)) => {
            info!(evm_inbox = %inbox, "EVM destination enabled");
            router = router.with_evm(Box::new(EvmDestSubmitter::new(rpc, inbox, key)?));
        }
        _ => info!("EVM destination not configured"),
    }
    match (
        &cfg.sui_dest_rpc_url,
        &cfg.sui_relayer_key,
        &cfg.sui_inbox_id,
        &cfg.sui_group_key_registry_id,
    ) {
        (Some(rpc), Some(key), Some(inbox), Some(keys)) => {
            info!("Sui destination enabled (type-derived bridge_receive)");
            let s = SuiDestSubmitter::connect(rpc, key, inbox, keys, cfg.sui_gas_budget)
                .await
                .context("connecting Sui destination submitter")?;
            router = router.with_sui(Box::new(s));
        }
        _ => info!("Sui destination not configured"),
    }
    let router = Arc::new(router);
    let signer = Arc::new(HttpSigner::new(&cfg.signer_url));
    let interval = Duration::from_secs(cfg.poll_interval_secs);

    // --- source watchers (each direction's origin) ---
    let mut tasks = Vec::new();

    let sui_watcher: Box<dyn SourceWatcher> =
        Box::new(SuiSourceWatcher::connect(&cfg.source_rpc_url, &cfg.bridge_package_id, domain_sep).await?);
    tasks.push(spawn_watcher("sui-source", sui_watcher, domain_sep, signer.clone(), router.clone(), interval));

    if let (Some(rpc), Some(outbox)) = (&cfg.evm_source_rpc_url, &cfg.evm_source_outbox_addr) {
        info!(evm_outbox = %outbox, "EVM source watcher enabled");
        let w = EvmSourceWatcher::connect(
            rpc,
            outbox,
            domain_sep,
            cfg.evm_source_confirmations,
            cfg.evm_source_start_block,
        )
        .await
        .context("connecting EVM source watcher")?;
        tasks.push(spawn_watcher("evm-source", Box::new(w), domain_sep, signer.clone(), router.clone(), interval));
    }

    info!(sources = tasks.len(), "bridge-relayer started");
    // If any watcher loop exits, the process exits (they loop forever normally).
    futures_join(tasks).await;
    Ok(())
}

fn spawn_watcher(
    name: &'static str,
    mut watcher: Box<dyn SourceWatcher>,
    domain_sep: bridge_types::Bytes32,
    signer: Arc<HttpSigner>,
    router: Arc<Router>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match relay_once(watcher.as_mut(), &domain_sep, signer.as_ref(), router.as_ref()).await {
                Ok(n) if n > 0 => info!(source = name, delivered = n, "poll complete"),
                Ok(_) => {}
                Err(e) => error!(source = name, error = %format!("{e:#}"), "poll failed, retrying"),
            }
            tokio::time::sleep(interval).await;
        }
    })
}

async fn futures_join(tasks: Vec<tokio::task::JoinHandle<()>>) {
    for t in tasks {
        let _ = t.await;
    }
}
