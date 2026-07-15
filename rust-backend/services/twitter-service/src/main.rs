use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use twitter_service::twitter::TwitterClient;
use twitter_service::{router, AppState, Cli, Config, TwitterSecrets};

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("twitter-service");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    let secrets = TwitterSecrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;

    let twitter = TwitterClient::new(&cfg.twitter_api_base)?;

    info!(
        environment = %cfg.environment,
        accounts = %secrets.accounts.keys().cloned().collect::<Vec<_>>().join(","),
        "twitter-service starting"
    );

    let state = Arc::new(AppState {
        twitter,
        accounts: secrets.accounts,
    });

    router::serve(cfg.bind_addr, state).await
}
