use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use social_bot::twitter_client::TwitterServiceClient;
use social_bot::{discord, router, AppState, BotSecrets, Cli, Config};

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("social-bot");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    let secrets = BotSecrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;

    let discord_verify_key = discord::parse_public_key(&secrets.discord_public_key)
        .context("parsing discord public key from secrets")?;

    let twitter = TwitterServiceClient::new(&cfg.twitter_service_url)?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("building follow-up http client")?;

    info!(
        environment = %cfg.environment,
        twitter_service_url = %cfg.twitter_service_url,
        slack_allowed = cfg.slack_allowed_user_ids.len(),
        discord_allowed = cfg.discord_allowed_user_ids.len(),
        "social-bot starting"
    );

    let state = Arc::new(AppState {
        twitter,
        http,
        slack_signing_secret: secrets.slack_signing_secret,
        discord_verify_key,
        slack_allowed_user_ids: cfg.slack_allowed_user_ids,
        discord_allowed_user_ids: cfg.discord_allowed_user_ids,
    });

    router::serve(cfg.bind_addr, state).await
}
