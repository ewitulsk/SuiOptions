use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{error, info};

use solana_deployments::SolanaDeployments;
use solana_token_info::db::{establish_pool, run_migrations, Repo};
use solana_token_info::{overlay, router, AppState, Cli, Config};

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("solana-token-info");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    // solana-token-info is the ONLY service that reads
    // solana-deployments.json. Pull the program_info for the configured env
    // and keep it resident. An un-deployed env (null slot) errors here —
    // failing at boot is correct until an operator deploys.
    let deployments = SolanaDeployments::load(&cfg.deployments_path).with_context(|| {
        format!(
            "loading solana deployments from {}",
            cfg.deployments_path.display()
        )
    })?;
    let net = deployments.for_env(&cfg.environment).with_context(|| {
        format!(
            "resolving env {} in solana-deployments.json",
            cfg.environment
        )
    })?;
    net.validate()
        .context("validating solana-deployments.json ids")?;
    let program_info = net.program_info.clone();
    info!(
        environment = %cfg.environment,
        network = %program_info.network,
        core_program = %program_info.options_core_program_id,
        "loaded program_info from solana-deployments.json"
    );

    // Non-mainnet-beta: derive the read-time test-token overlay merged into
    // `/tokens`. Mainnet-beta gets an empty overlay (DB catalog only).
    let overlay = overlay::build(net, &cfg.seed_meta, cfg.overlay_test_tokens());
    if !cfg.overlay_test_tokens() {
        info!(network = %cfg.network, "test-token overlay disabled (mainnet-beta)");
    }

    // Stand up the catalog DB and apply migrations before serving.
    let pool = Arc::new(
        establish_pool(&cfg.database_url, cfg.db_pool_size).context("establish_pool")?,
    );
    run_migrations(&pool).context("run_migrations")?;
    let repo = Repo::new(pool);
    info!(pool_size = cfg.db_pool_size, "postgres pool ready");

    let state = Arc::new(AppState::new(repo, program_info, overlay));

    // Admin-JWT gate for the public mutate routes. solana-token-info never
    // validates tokens itself — this client delegates to
    // solana-auth-service's /verify.
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
