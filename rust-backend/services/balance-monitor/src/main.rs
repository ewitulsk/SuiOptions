use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use sui_sdk::{SuiClient, SuiClientBuilder};
use sui_types::base_types::SuiAddress;
use sui_types::crypto::SuiKeyPair;
use tracing::{error, info, warn};

use balance_monitor::{config::Watch, Cli, Config};

const MIST_PER_SUI: f64 = 1_000_000_000.0;

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("balance-monitor");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    let readiness = observability::ops::Readiness::new();
    observability::ops::spawn(cfg.ops_addr, &readiness);

    // One-shot alert-pipeline test: `ALERT_TEST=1` emits the canonical test
    // alert_id so the Loki → Grafana alert rule can be verified end-to-end.
    if std::env::var("ALERT_TEST").as_deref() == Ok("1") {
        tracing::error!(
            alert_id = "this-is-a-test-alert",
            "test alert emitted (ALERT_TEST=1)"
        );
    }

    // Prefer the shared `[sui] rpc_url` override from the optional secrets file
    // (rendered by render-secrets.sh) over the public network default. The
    // per-watch secrets files (resolved below) are a separate concern.
    // Optional: a missing/unreadable file degrades to the public endpoint.
    let rpc_url = match cli.secrets.as_deref() {
        Some(path) => match runtime_config::Secrets::load(path) {
            Ok(s) => s.resolve_rpc_url(cfg.network.rpc_url()),
            Err(e) => {
                warn!(error = %e, path = %path.display(), "secrets file unreadable; using public RPC");
                cfg.network.rpc_url().to_string()
            }
        },
        None => cfg.network.rpc_url().to_string(),
    };
    info!(rpc = %redact_rpc(&rpc_url), "resolved Sui JSON-RPC endpoint");
    let sui = SuiClientBuilder::default()
        .build(&rpc_url)
        .await
        .context("connecting Sui client")?;

    // Resolve each watch to an address. Watches whose secrets file is absent
    // are skipped with a warning — that's how a service opts out of an env
    // (e.g. mm-bot in prod), mirroring render-secrets.sh.
    let mut targets: Vec<(Watch, SuiAddress)> = Vec::new();
    for w in &cfg.watches {
        match resolve_address(w, &cfg) {
            Ok(addr) => {
                info!(service = %w.name, address = %addr, threshold_sui = w.low_balance_sui, "watching wallet");
                targets.push((w.clone(), addr));
            }
            Err(e) => {
                warn!(service = %w.name, error = %e, "watch skipped: address unresolved");
            }
        }
    }
    if targets.is_empty() {
        anyhow::bail!("no watchable wallets resolved");
    }

    info!(
        environment = %cfg.environment,
        network = %cfg.network,
        wallets = targets.len(),
        interval_secs = cfg.poll_interval_secs,
        "balance-monitor starting"
    );

    // Config, the RPC client and watch resolution are behind us; the poll loop
    // below only warns. balance-monitor has no entry in `deploy.sh`'s
    // `health_path_for`, so nothing gates on this today — it is updated
    // because `ops::spawn` now requires the handle, and flipping it in the
    // right place costs nothing and is correct if it is ever gated (SO-324).
    readiness.ready();

    let mut tick = tokio::time::interval(Duration::from_secs(cfg.poll_interval_secs));
    loop {
        tick.tick().await;
        for (watch, addr) in &targets {
            poll_wallet(&sui, watch, *addr).await;
        }
    }
}

/// Strip any token-bearing path from an RPC URL for logging, keeping the host.
fn redact_rpc(url: &str) -> &str {
    url.split("://")
        .nth(1)
        .and_then(|s| s.split('/').next())
        .unwrap_or(url)
}

fn resolve_address(w: &Watch, cfg: &Config) -> Result<SuiAddress> {
    if let Some(addr) = &w.address {
        return SuiAddress::from_str(addr)
            .map_err(|e| anyhow!("watch '{}': bad address: {e}", w.name));
    }
    let path = w.secrets_file.as_ref().expect("validated in Config::load");
    let secrets = runtime_config::Secrets::load(path)?;
    let key = secrets.sui_private_key(cfg.network.as_str())?;
    let kp = SuiKeyPair::decode(key)
        .map_err(|e| anyhow!("watch '{}': decoding SUI private key: {e}", w.name))?;
    Ok(SuiAddress::from(&kp.public()))
}

async fn poll_wallet(sui: &SuiClient, watch: &Watch, addr: SuiAddress) {
    let balance = match sui.coin_read_api().get_balance(addr, None).await {
        Ok(b) => b,
        Err(e) => {
            warn!(service = %watch.name, address = %addr, error = %e, "balance poll failed");
            metrics::counter!("balance_monitor_poll_errors_total", "service" => watch.name.clone())
                .increment(1);
            return;
        }
    };

    let sui_balance = balance.total_balance as f64 / MIST_PER_SUI;
    let addr_str = addr.to_string();
    metrics::gauge!(
        "sui_balance_sui",
        "service" => watch.name.clone(),
        "address" => addr_str.clone(),
    )
    .set(sui_balance);

    let low = sui_balance < watch.low_balance_sui;
    metrics::gauge!("sui_balance_low", "service" => watch.name.clone()).set(low as u8 as f64);

    if low {
        // Fires the generic alert_id Grafana rule on top of the gauge rule.
        error!(
            alert_id = format!("low-balance-{}", watch.name),
            service = %watch.name,
            address = %addr_str,
            balance_sui = sui_balance,
            threshold_sui = watch.low_balance_sui,
            "SUI balance below threshold"
        );
    }
}
