use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use engagement_service::db::{establish_pool, repo::Repo, run_migrations};
use engagement_service::twitter_client::TwitterServiceClient;
use engagement_service::{poller, router, AppState, Cli, Config};

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("engagement-service");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    let pool = Arc::new(establish_pool(&cfg.database_url, cfg.db_pool_size)?);
    run_migrations(&pool).context("running engagement DB migrations")?;
    let repo = Repo::new(pool);
    info!(pool_size = cfg.db_pool_size, "engagement DB ready (migrations applied)");

    let twitter = TwitterServiceClient::new(&cfg.twitter_service_url)?;

    info!(
        environment = %cfg.environment,
        twitter_service_url = %cfg.twitter_service_url,
        account = %cfg.twitter_account,
        poll_interval_secs = cfg.poll_interval_secs,
        ambassadors = cfg.points.ambassadors.len(),
        "engagement-service starting"
    );

    let state = Arc::new(AppState { repo, twitter, cfg });
    poller::spawn(Arc::clone(&state));

    let bind_addr = state.cfg.bind_addr;
    router::serve(bind_addr, state).await
}
