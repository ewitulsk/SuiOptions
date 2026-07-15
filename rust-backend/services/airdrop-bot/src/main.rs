use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use airdrop_bot::engagement_client::EngagementClient;
use airdrop_bot::{discord, router, AppState, BotSecrets, Cli, Config};

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("airdrop-bot");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    let secrets = BotSecrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;

    let discord_verify_key = discord::parse_public_key(&secrets.discord_public_key)
        .context("parsing discord public key from secrets")?;

    let engagement = EngagementClient::new(&cfg.engagement_service_url)?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("building follow-up http client")?;

    info!(
        environment = %cfg.environment,
        engagement_service_url = %cfg.engagement_service_url,
        "airdrop-bot starting"
    );

    let state = Arc::new(AppState {
        engagement,
        http,
        discord_verify_key,
    });

    router::serve(cfg.bind_addr, state).await
}
