//! solana-auth-service.
//!
//! Issues short-lived admin JWTs to Solana wallets on a hardcoded allowlist,
//! after an ed25519 signature challenge-response. It is the single holder of
//! the JWT signing secret: other services gate endpoints by calling the
//! internal `/verify` route (via `auth-client`) rather than verifying tokens
//! themselves.
//!
//! Two routers on two ports:
//! - public (proxied by nginx): `GET /challenge`, `POST /login`,
//!   `POST /refresh`.
//! - internal (network-isolated): `POST /verify`.

pub mod allowlist;
pub mod challenge;
pub mod config;
pub mod handlers;
pub mod jwt;
pub mod router;
pub mod solana_sig;
pub mod state;

pub use config::Config;
pub use state::AppState;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "solana-auth-service",
    about = "Solana-wallet admin auth. Issues + verifies short-lived JWTs for an allowlist of addresses."
)]
pub struct Cli {
    #[arg(short, long, default_value = "services/solana-auth-service/config/config.toml")]
    pub config: PathBuf,

    /// Secrets TOML holding `auth.jwt_secret`. No env-var fallback.
    #[arg(short = 's', long, default_value = "services/solana-auth-service/config/secrets.toml")]
    pub secrets: PathBuf,
}

cli_spec::define_program! {
    id          = "solana-auth-service",
    cargo_pkg   = "solana-auth-service",
    working_dir = ".",
    description = "Solana-wallet admin auth-service. Challenge-response login against a hardcoded \
                   address allowlist, issues HS256 JWTs, and exposes an internal verify route \
                   other services delegate to.",
    cli         = crate::Cli,
}
