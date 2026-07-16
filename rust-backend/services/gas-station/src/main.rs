use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use gas_station::{router, AppState, Cli, Config};
use sui_tx::tx::sponsor::BudgetPolicy;
use sui_tx::tx::template::protocol_templates;
use sui_tx::{Network, SuiClientWrapper};
use sui_types::base_types::ObjectID;
use token_info_client::TokenInfoClient;

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("gas-station");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;

    let sui = SuiClientWrapper::connect(&secrets, cfg.network)
        .await
        .context("connecting Sui client + sponsor signer")?;

    // Build the sponsored-PTB templates from token-info: the protocol package
    // (the `${pkg}` in every frontend builder) plus, on dev/staging, the
    // test-token packages/modules the faucet mints from. token-info is a hard
    // dependency — if it's unreachable at boot we crash rather than sponsor a
    // stale or empty set.
    let snapshot = TokenInfoClient::new(&cfg.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching token-info from {}", cfg.token_info_url))?;

    let protocol = snapshot
        .package()
        .context("protocol package id from token-info")?;

    // Vault flows live in the options_vault package (four-package split);
    // a deployment without it can't sponsor vault PTBs, so fail at boot.
    let vault_pkg = snapshot
        .vault()
        .context("options_vault package missing from token-info package_info")?
        .package()
        .context("vault package id from token-info")?;

    // Faucet `mint_to_sender` is testnet-only; sponsor it on any non-mainnet
    // network (prod is itself a testnet deployment that ships test tokens).
    let allow_faucet = cfg.network != Network::Mainnet;
    let mut test_tokens: Vec<(ObjectID, String)> = Vec::new();
    if allow_faucet {
        if let Some(tt) = snapshot.maybe_test_tokens() {
            for symbol in tt.symbols() {
                let info = tt
                    .get(symbol)
                    .with_context(|| format!("test token {symbol}"))?;
                let (pkg, module) = info
                    .module_path()
                    .with_context(|| format!("module path for test token {symbol}"))?;
                test_tokens.push((pkg, module));
            }
        }
    }

    // DeepBook PTBs (SO-154) sponsor only where token-info reports a
    // deployment; calls target the upgraded package id.
    let deepbook = snapshot
        .deepbook()
        .map(|d| d.package())
        .transpose()
        .context("deepbook package id from token-info")?;

    // CCTP bridge PTBs sponsor only where Circle's TokenMessengerMinter is
    // configured for this network; the burn calls Circle directly.
    let cctp = cfg
        .cctp_token_messenger_package
        .as_deref()
        .map(|tmm| ObjectID::from_hex_literal(tmm).context("bad cctp_token_messenger_package"))
        .transpose()?;

    let templates =
        protocol_templates(protocol, vault_pkg, &test_tokens, allow_faucet, deepbook, cctp);

    info!(
        environment = %cfg.environment,
        network = %cfg.network,
        sponsor = %sui.signer.address,
        templates = templates.len(),
        deepbook = deepbook.is_some(),
        faucet_tokens = test_tokens.len(),
        threshold_mist = cfg.min_balance_threshold_mist,
        max_gas_budget_mist = cfg.max_gas_budget_mist,
        "gas-station starting"
    );

    let state = Arc::new(AppState {
        sui,
        templates,
        policy: BudgetPolicy {
            max_gas_budget: cfg.max_gas_budget_mist,
            min_gas_budget: cfg.min_gas_budget_mist,
            buffer_bps: cfg.gas_budget_buffer_bps,
        },
        min_balance_threshold_mist: cfg.min_balance_threshold_mist,
    });

    router::serve(cfg.bind_addr, state, &cfg.allowed_origins).await
}
