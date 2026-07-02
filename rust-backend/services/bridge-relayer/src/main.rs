use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{error, info};

use bridge_relayer::evm_submit::EvmDestSubmitter;
use bridge_relayer::relay::{relay_once, DestSubmitter};
use bridge_relayer::signer_client::HttpSigner;
use bridge_relayer::submit::DryRunSubmitter;
use bridge_relayer::sui_source::SuiSourceWatcher;
use bridge_relayer::{Cli, Config};

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("bridge-relayer");

    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)
        .with_context(|| format!("loading config from {}", cli.config.display()))?;

    let mut watcher =
        SuiSourceWatcher::connect(&cfg.source_rpc_url, &cfg.bridge_package_id).await?;
    let signer = HttpSigner::new(&cfg.signer_url);

    // Real EVM submitter when configured, else dry-run.
    let submitter: Box<dyn DestSubmitter> =
        match (&cfg.evm_rpc_url, &cfg.evm_inbox_addr, &cfg.evm_relayer_key) {
            (Some(rpc), Some(inbox), Some(key)) => {
                info!(evm_inbox = %inbox, "EVM destination submitter enabled");
                Box::new(EvmDestSubmitter::new(rpc, inbox, key)?)
            }
            _ => {
                info!("EVM destination not configured — using dry-run submitter");
                Box::new(DryRunSubmitter)
            }
        };

    info!(
        source_rpc_url = %cfg.source_rpc_url,
        bridge_package_id = %cfg.bridge_package_id,
        signer_url = %cfg.signer_url,
        poll_interval_secs = cfg.poll_interval_secs,
        "bridge-relayer started (M1: Sui source watch + sign; destination submission is dry-run)"
    );

    let interval = Duration::from_secs(cfg.poll_interval_secs);
    loop {
        match relay_once(&mut watcher, &signer, submitter.as_ref()).await {
            Ok(n) if n > 0 => info!(delivered = n, "poll complete"),
            Ok(_) => {}
            Err(e) => error!(error = %e, "poll failed, retrying after interval"),
        }
        tokio::time::sleep(interval).await;
    }
}
