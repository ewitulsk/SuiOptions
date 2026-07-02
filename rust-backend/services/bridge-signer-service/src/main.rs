use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{error, info};

use bridge_signer::ThresholdSigner;
use bridge_signer_service::{router, verifier, AppState, Cli, Config};

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("bridge-signer-service");

    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)
        .with_context(|| format!("loading config from {}", cli.config.display()))?;

    let signer = ThresholdSigner::from_seeds(cfg.ed25519_seed()?, cfg.secp256k1_seed()?)
        .context("building signer from seeds")?;
    info!(
        ed25519_group_pubkey = %format!("0x{}", hex::encode(signer.ed25519_group_pubkey())),
        ecdsa_group_address = %format!("0x{}", hex::encode(signer.ecdsa_group_address())),
        "signer keys loaded (M1 single-party)"
    );

    let verifier = verifier::build(&cfg.source_verifier).context("building source verifier")?;

    let state = Arc::new(AppState {
        signer,
        verifier,
        ed25519_group_pubkey_id: cfg.ed25519_group_pubkey_id,
        ecdsa_group_pubkey_id: cfg.ecdsa_group_pubkey_id,
    });

    let public_addr = cfg.public_bind_addr;
    let admin_addr = cfg.admin_bind_addr;
    let public = tokio::spawn(router::serve_public(public_addr, Arc::clone(&state)));
    let admin = tokio::spawn(router::serve_admin(admin_addr));

    tokio::select! {
        res = public => match res {
            Ok(Ok(())) => info!("public API finished"),
            Ok(Err(e)) => error!(error = %e, "public API exited"),
            Err(e) => error!(error = %e, "public API task panicked"),
        },
        res = admin => match res {
            Ok(Ok(())) => info!("admin API finished"),
            Ok(Err(e)) => error!(error = %e, "admin API exited"),
            Err(e) => error!(error = %e, "admin API task panicked"),
        },
    }
    Ok(())
}
