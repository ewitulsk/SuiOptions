use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{error, info};

use deployments::Deployments;
use token_info::db::{establish_pool, run_migrations, Repo};
use token_info::{overlay, router, AppState, Cli, Config};

#[tokio::main]
async fn main() -> Result<()> {
    runtime_config::logging::init();

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    // token-info is the ONLY service that reads deployments.json. Pull the
    // package_info for the configured network and keep it resident.
    let deployments = Deployments::load(&cfg.deployments_path).with_context(|| {
        format!("loading deployments from {}", cfg.deployments_path.display())
    })?;
    let net = deployments
        .for_network(&cfg.network)
        .with_context(|| format!("resolving network {} in deployments.json", cfg.network))?;
    let package_info = net.package_info.clone();
    info!(
        network = %cfg.network,
        package_id = %package_info.package_id,
        "loaded package_info from deployments.json"
    );

    // Dev/staging: derive the read-time test-token overlay merged into
    // `/tokens`. Prod gets an empty overlay (DB catalog only).
    let overlay = overlay::build(net, &cfg.seed_meta, cfg.overlay_test_tokens());
    if !cfg.overlay_test_tokens() {
        info!(environment = %cfg.environment, "test-token overlay disabled (not dev/staging)");
    }

    // Stand up the catalog DB and apply migrations before serving.
    let pool = Arc::new(
        establish_pool(&cfg.database_url, cfg.db_pool_size).context("establish_pool")?,
    );
    run_migrations(&pool).context("run_migrations")?;
    let repo = Repo::new(pool);
    info!(pool_size = cfg.db_pool_size, "postgres pool ready");

    let state = Arc::new(AppState::new(repo, package_info, overlay));

    // Admin-JWT gate for the public mutate routes. token-info never validates
    // tokens itself — this client delegates to auth-service's /verify.
    let auth = Arc::new(auth_client::AuthClient::new(&cfg.auth_service_url));
    info!(auth_service_url = %cfg.auth_service_url, "auth delegation configured");

    // Public read API + internal mutate API on separate ports.
    let public_state = Arc::clone(&state);
    let public_addr = cfg.public_bind_addr;
    let origins = cfg.allowed_origins.clone();
    let public_auth = Arc::clone(&auth);
    let public = tokio::spawn(async move {
        router::serve_public(public_addr, public_state, &origins, public_auth).await
    });

    let internal_state = Arc::clone(&state);
    let internal_addr = cfg.internal_bind_addr;
    let internal = tokio::spawn(async move {
        router::serve_internal(internal_addr, internal_state).await
    });

    tokio::select! {
        res = public => match res {
            Ok(Ok(())) => info!("public API finished"),
            Ok(Err(e)) => error!(error = %e, "public API exited"),
            Err(e) => error!(error = %e, "public API task panicked"),
        },
        res = internal => match res {
            Ok(Ok(())) => info!("internal API finished"),
            Ok(Err(e)) => error!(error = %e, "internal API exited"),
            Err(e) => error!(error = %e, "internal API task panicked"),
        },
    }
    Ok(())
}
