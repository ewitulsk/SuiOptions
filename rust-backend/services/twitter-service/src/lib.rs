//! twitter-service.
//!
//! Manages outgoing tweets for multiple Twitter/X accounts. Each account's
//! OAuth 1.0a user-context credentials live in the secrets TOML (`--secrets`,
//! rendered from AWS Secrets Manager in deployed envs); tweets are posted via
//! the Twitter API v2 `POST /2/tweets` endpoint signed per-account.
//!
//! Internal-only: the bind port is reachable on the compose `net` network
//! (e.g. by social-bot) and is deliberately never proxied by nginx.
//!
//! Endpoints:
//! - `GET /health`
//! - `GET /accounts` — the configured account names.
//! - `POST /tweets` — `{account, text}` → post a tweet from that account.
//! - `GET /mentions?account=…[&since_id=…]` — recent tweets mentioning
//!   `@account` with engagement counters (consumed by engagement-service).
//! - `GET /tweets/metrics?account=…&ids=…` — refresh counters for up to
//!   100 known tweets.

pub mod config;
pub mod handlers;
pub mod oauth1;
pub mod router;
pub mod secrets;
pub mod state;
pub mod twitter;

pub use config::Config;
pub use secrets::TwitterSecrets;
pub use state::AppState;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "twitter-service",
    about = "Posts outgoing tweets for multiple Twitter/X accounts."
)]
pub struct Cli {
    #[arg(
        short,
        long,
        default_value = "services/twitter-service/config/config.toml"
    )]
    pub config: PathBuf,

    /// Secrets TOML holding the per-account `[accounts.<name>]` OAuth 1.0a
    /// credentials. No env-var fallback.
    #[arg(
        short = 's',
        long,
        default_value = "services/twitter-service/config/secrets.toml"
    )]
    pub secrets: PathBuf,
}

cli_spec::define_program! {
    id          = "twitter-service",
    cargo_pkg   = "twitter-service",
    working_dir = ".",
    description = "Twitter service. Manages outgoing tweets for multiple accounts: signs \
                   Twitter API v2 create-tweet requests with per-account OAuth 1.0a \
                   credentials loaded from the secrets TOML.",
    cli         = crate::Cli,
}
