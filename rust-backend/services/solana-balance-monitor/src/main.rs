use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use tracing::{error, info, warn};

use solana_balance_monitor::{config::Watch, Cli, Config};
use solana_tx::{Network, Signer};

const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("solana-balance-monitor");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    observability::ops::spawn(cfg.ops_addr);

    // One-shot alert-pipeline test: `ALERT_TEST=1` emits the canonical test
    // alert_id so the Loki → Grafana alert rule can be verified end-to-end.
    if std::env::var("ALERT_TEST").as_deref() == Ok("1") {
        tracing::error!(
            alert_id = "this-is-a-test-alert",
            "test alert emitted (ALERT_TEST=1)"
        );
    }

    // Prefer the shared `[solana] rpc_url` override from the optional secrets
    // file (rendered by render-secrets.sh) over the public cluster default.
    // The per-watch secrets files (resolved below) are a separate concern.
    // Optional: a missing/unreadable file degrades to the public endpoint.
    let rpc_url = match cli.secrets.as_deref() {
        Some(path) => match runtime_config::Secrets::load(path) {
            Ok(s) => s.resolve_solana_rpc_url(cfg.network.rpc_url()),
            Err(e) => {
                warn!(error = %e, path = %path.display(), "secrets file unreadable; using public RPC");
                cfg.network.rpc_url().to_string()
            }
        },
        None => cfg.network.rpc_url().to_string(),
    };
    info!(rpc_host = %redact_rpc(&rpc_url), "resolved Solana JSON-RPC endpoint");
    let rpc = RpcClient::new(rpc_url);

    // Resolve each watch to an address. Watches whose secrets file is absent
    // are skipped with a warning — that's how a service opts out of an env
    // (e.g. mm-bot), mirroring render-secrets.sh.
    let mut targets: Vec<(Watch, Pubkey)> = Vec::new();
    for w in &cfg.watches {
        match resolve_address(w, cfg.network) {
            Ok(addr) => {
                info!(service = %w.name, address = %addr, threshold_sol = w.low_balance_sol, "watching wallet");
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
        "solana-balance-monitor starting"
    );

    let mut tick = tokio::time::interval(Duration::from_secs(cfg.poll_interval_secs));
    loop {
        tick.tick().await;
        for (watch, addr) in &targets {
            poll_wallet(&rpc, watch, *addr).await;
        }
    }
}

/// Strip any key-bearing path/query from an RPC URL for logging, keeping the
/// host (Helius URLs carry the API key as a query param).
fn redact_rpc(url: &str) -> &str {
    url.split("://")
        .nth(1)
        .and_then(|s| s.split(['/', '?']).next())
        .unwrap_or(url)
}

fn resolve_address(w: &Watch, network: Network) -> Result<Pubkey> {
    if let Some(addr) = &w.address {
        return Pubkey::from_str(addr)
            .map_err(|e| anyhow!("watch '{}': bad address: {e}", w.name));
    }
    let path = w.secrets_file.as_ref().expect("validated in Config::load");
    let secrets = runtime_config::Secrets::load(path)?;
    let key = secrets.solana_keypair(network.as_str())?;
    let signer = Signer::from_string(key)
        .map_err(|e| anyhow!("watch '{}': decoding solana keypair: {e}", w.name))?;
    Ok(signer.pubkey())
}

fn lamports_to_sol(lamports: u64) -> f64 {
    lamports as f64 / LAMPORTS_PER_SOL
}

async fn poll_wallet(rpc: &RpcClient, watch: &Watch, addr: Pubkey) {
    let lamports = match rpc.get_balance(&addr).await {
        Ok(l) => l,
        Err(e) => {
            warn!(service = %watch.name, address = %addr, error = %e, "balance poll failed");
            metrics::counter!("solana_balance_monitor_poll_errors_total", "service" => watch.name.clone())
                .increment(1);
            return;
        }
    };

    let sol_balance = lamports_to_sol(lamports);
    let addr_str = addr.to_string();
    metrics::gauge!(
        "sol_balance_sol",
        "service" => watch.name.clone(),
        "address" => addr_str.clone(),
    )
    .set(sol_balance);

    let low = sol_balance < watch.low_balance_sol;
    metrics::gauge!("sol_balance_low", "service" => watch.name.clone()).set(low as u8 as f64);

    if low {
        // Fires the generic alert_id Grafana rule on top of the gauge rule.
        error!(
            alert_id = format!("low-balance-{}", watch.name),
            service = %watch.name,
            address = %addr_str,
            balance_sol = sol_balance,
            threshold_sol = watch.low_balance_sol,
            "SOL balance below threshold"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::signature::Keypair;
    use solana_sdk::signer::Signer as _;
    use std::path::PathBuf;

    fn watch(secrets_file: Option<PathBuf>, address: Option<&str>) -> Watch {
        Watch {
            name: "test".into(),
            secrets_file,
            address: address.map(str::to_string),
            low_balance_sol: 1.0,
        }
    }

    #[test]
    fn lamports_convert_to_sol() {
        assert_eq!(lamports_to_sol(0), 0.0);
        assert_eq!(lamports_to_sol(1_000_000_000), 1.0);
        assert_eq!(lamports_to_sol(1_500_000_000), 1.5);
        assert_eq!(lamports_to_sol(1), 1e-9);
    }

    #[test]
    fn resolves_explicit_address() {
        let pk = Pubkey::new_unique();
        let w = watch(None, Some(&pk.to_string()));
        assert_eq!(resolve_address(&w, Network::Devnet).unwrap(), pk);
    }

    #[test]
    fn rejects_bad_address() {
        let w = watch(None, Some("not-a-pubkey-!!!"));
        let err = resolve_address(&w, Network::Devnet).unwrap_err().to_string();
        assert!(err.contains("bad address"), "{err}");
    }

    #[test]
    fn resolves_address_from_secrets_file() {
        let kp = Keypair::new();
        let b58 = bs58::encode(kp.to_bytes()).into_string();
        let path = std::env::temp_dir().join(format!(
            "solana-balance-monitor-resolve-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, format!("[solana]\ndevnet = \"{b58}\"\n")).unwrap();

        let w = watch(Some(path.clone()), None);
        assert_eq!(resolve_address(&w, Network::Devnet).unwrap(), kp.pubkey());
        // Wrong network slot with no default fallback → error, watch skipped.
        assert!(resolve_address(&w, Network::Testnet).is_err());
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn missing_secrets_file_errors() {
        let w = watch(Some(PathBuf::from("/nonexistent/secrets.toml")), None);
        assert!(resolve_address(&w, Network::Devnet).is_err());
    }

    #[test]
    fn redacts_rpc_urls() {
        assert_eq!(
            redact_rpc("https://devnet.helius-rpc.com/?api-key=secret"),
            "devnet.helius-rpc.com"
        );
        assert_eq!(
            redact_rpc("https://api.devnet.solana.com"),
            "api.devnet.solana.com"
        );
    }
}
