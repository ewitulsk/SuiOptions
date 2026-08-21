//! dakota-service binary.
//!
//! Boot order follows the house pattern: logging → config (`${VAR}` expansion)
//! → DB pool + embedded migrations → secrets (the Dakota API key — a hard
//! requirement, nothing works without it) → clients → axum serve.

use std::sync::Arc;

use anyhow::{Context, Result};
use auth_client::AuthClient;
use clap::Parser;
use tracing::{info, warn};

use dakota_service::dakota::DakotaClient;
use dakota_service::db::{establish_pool, repo::Repo, run_migrations};
use dakota_service::invites::InviteClient;
use dakota_service::state::AppState;
use dakota_service::wallet::WalletSigner;
use dakota_service::{router, webhook, Cli, Config};

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("dakota-service");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    let pool = Arc::new(establish_pool(&cfg.database_url, cfg.db_pool_size)?);
    run_migrations(&pool).context("running dakota-service DB migrations")?;
    let repo = Repo::new(Arc::clone(&pool));
    info!(pool_size = cfg.db_pool_size, "dakota-service DB ready (migrations applied)");

    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;
    let api_key = secrets
        .dakota_api_key()
        .context("resolving the dakota api key")?;

    // Parsed at boot so a malformed key is a startup failure rather than a
    // silent flood of rejected deliveries hours later.
    let webhook_key = webhook::parse_verifying_key(&cfg.dakota.webhook_public_key)
        .context("parsing dakota.webhook_public_key")?;

    let dakota = DakotaClient::new(&cfg.dakota.base_url, api_key);
    let auth = Arc::new(AuthClient::new(cfg.auth.internal_url.clone()));
    let invites = InviteClient::new(cfg.auth.internal_url.clone(), cfg.auth.invite_ttl_secs);

    if cfg.dakota.webhook_url.is_none() {
        warn!(
            "dakota.webhook_url is unset — POST /admin/webhooks/register will fail and no \
             events will arrive until it is configured"
        );
    }
    if cfg.dakota.allowed_networks.is_empty() {
        warn!(
            "dakota.allowed_networks is empty — every network Dakota reports will be offered, \
             including mainnets the sandbox then refuses"
        );
    }

    let bind_addr = cfg.bind_addr;
    let origins = cfg.allowed_origins.clone();
    info!(
        environment = %cfg.environment,
        dakota = %cfg.dakota.base_url,
        max_amount_minor = cfg.dakota.max_amount_minor,
        "dakota-service starting"
    );

    // Optional: only the treasury needs it, and a service with no treasury key
    // is still fully useful for onboarding and ramps.
    let wallet_signer = match secrets.dakota_wallet_p256_pem() {
        Some(pem) => {
            let s = WalletSigner::from_pem(pem).context("parsing dakota.wallet_p256_pem")?;
            info!(public_key = %s.public_key_b64()?, "treasury signing key loaded");
            Some(s)
        }
        None => {
            warn!("dakota.wallet_p256_pem is unset — the treasury endpoints will refuse");
            None
        }
    };

    let state = Arc::new(AppState::new(
        cfg,
        repo,
        dakota,
        webhook_key,
        invites,
        wallet_signer,
    ));
    router::serve(bind_addr, state, auth, &origins).await
}
