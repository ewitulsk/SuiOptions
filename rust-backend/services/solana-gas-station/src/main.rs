use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use solana_sdk::pubkey::Pubkey;
use tracing::{info, warn};

use solana_gas_station::sponsor::SponsorPolicy;
use solana_gas_station::template::protocol_templates;
use solana_gas_station::{faucet, router, AppState, Cli, Config};
use solana_token_info_client::TokenInfoClient;
use solana_tx::SolanaClientWrapper;

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("solana-gas-station");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;

    let solana = SolanaClientWrapper::connect(&secrets, cfg.network)
        .context("connecting Solana client + station signer")?;
    let station = solana.signer.pubkey();

    // Hard cutover: the sponsored-flow templates are seeded by the
    // program ids solana-token-info reports. If it's unreachable at boot
    // we crash rather than sponsor a stale or empty set.
    let snapshot = TokenInfoClient::new(&cfg.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching solana-token-info from {}", cfg.token_info_url))?;

    if snapshot.network() != cfg.network.as_str() {
        warn!(
            config_network = %cfg.network,
            snapshot_network = %snapshot.network(),
            "config network differs from the deployment's network"
        );
    }

    let parse = |what: &str, s: &str| -> Result<Pubkey> {
        s.parse()
            .with_context(|| format!("{what} id from solana-token-info is not base58: {s}"))
    };
    let core = parse("options_core program", snapshot.core_program())?;
    let venue = parse("auction_venue program", snapshot.venue_program())?;
    let vault = parse("options_vault program", snapshot.vault_program())?;
    // The template discriminators come from the program crates in this
    // build; a program-id mismatch would mean the deployed programs are
    // not the sources we compiled against.
    if core != options_core::ID || venue != auction_venue::ID || vault != options_vault::ID {
        warn!(
            %core, %venue, %vault,
            "deployed program ids differ from the program crates' declare_id! — \
             template discriminators may be stale"
        );
    }
    let templates = protocol_templates(core, venue, vault);

    // Faucet: config-gated, force-off on mainnet-beta.
    let faucet_enabled = faucet::faucet_allowed(cfg.faucet_enabled, cfg.network);
    if cfg.faucet_enabled && !faucet_enabled {
        warn!("faucet_enabled is set but network is mainnet-beta; faucet force-disabled");
    }
    let faucet_tokens = if faucet_enabled {
        faucet::build_faucet_tokens(&snapshot, &cfg.faucet_amounts, &station)
            .context("building faucet token map")?
    } else {
        Default::default()
    };

    info!(
        environment = %cfg.environment,
        network = %cfg.network,
        station = %station,
        templates = templates.len(),
        faucet_enabled,
        faucet_tokens = faucet_tokens.len(),
        threshold_lamports = cfg.min_balance_threshold_lamports,
        max_sponsor_lamports_per_tx = cfg.max_sponsor_lamports_per_tx,
        "solana-gas-station starting"
    );

    let state = Arc::new(AppState {
        solana,
        templates,
        policy: SponsorPolicy {
            max_sponsor_lamports_per_tx: cfg.max_sponsor_lamports_per_tx,
            min_balance_threshold_lamports: cfg.min_balance_threshold_lamports,
        },
        faucet_enabled,
        faucet_tokens,
    });

    router::serve(cfg.bind_addr, state, &cfg.allowed_origins).await
}
