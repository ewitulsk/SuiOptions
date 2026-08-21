use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{error, info};

use auth_service::{router, AppState, Cli, Config};

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("auth-service");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;
    let jwt_secret = secrets.jwt_secret().context("auth.jwt_secret missing")?.to_string();

    info!(
        environment = %cfg.environment,
        admin_addresses = cfg.admin_addresses.len(),
        token_ttl_secs = cfg.token_ttl_secs,
        refresh_max_secs = cfg.refresh_max_secs,
        "auth-service starting"
    );
    // On-chain AdminCap fallback (SO-422): active only when both endpoints
    // are configured. Lazy — no boot-time dependency on token-info.
    let cap_check = match (&cfg.token_info_url, &cfg.sui_graphql_url) {
        (Some(ti), Some(gql)) => {
            info!(token_info = %ti, graphql = %gql, "AdminCap login fallback enabled");
            Some(auth_service::capcheck::CapCheck::new(ti, gql))
        }
        _ => {
            info!("AdminCap login fallback disabled (token_info_url / sui_graphql_url unset)");
            None
        }
    };
    if cfg.admin_addresses.is_empty() && cap_check.is_none() {
        tracing::warn!("admin_addresses is empty and cap check disabled — no wallet will be able to log in");
    }

    let state = Arc::new(AppState::new(
        jwt_secret,
        cfg.admin_addresses.clone(),
        cap_check,
        cfg.challenge_ttl_secs,
        cfg.token_ttl_secs,
        cfg.refresh_max_secs,
    ));

    let public_state = Arc::clone(&state);
    let public_addr = cfg.public_bind_addr;
    let origins = cfg.allowed_origins.clone();
    let public = tokio::spawn(async move {
        router::serve_public(public_addr, public_state, &origins).await
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
