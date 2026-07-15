//! social-bot.
//!
//! Slack + Discord slash-command bots in one deployment. An allow list of
//! user ids (per platform, in config) may post tweets from any account
//! twitter-service manages:
//!
//!   /tweet <account> <text…>
//!
//! Both platforms deliver commands as signed HTTP webhooks (no gateway
//! connections), proxied by nginx at /<env>/social-bot/:
//! - `POST /slack/command`       — Slack slash command (HMAC-SHA256 verified).
//! - `POST /discord/interactions`— Discord interactions endpoint (Ed25519
//!   verified).
//! - `GET /health`
//!
//! Slash-command flow: verify signature → check the allow list → ack within
//! the platform's 3s window → post via twitter-service in a background task →
//! deliver the result through the platform's follow-up hook (Slack
//! `response_url` / Discord interaction webhook).

pub mod commands;
pub mod config;
pub mod discord;
pub mod router;
pub mod secrets;
pub mod slack;
pub mod state;
pub mod twitter_client;

pub use config::Config;
pub use secrets::BotSecrets;
pub use state::AppState;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "social-bot",
    about = "Slack + Discord bots: allow-listed users post tweets via twitter-service."
)]
pub struct Cli {
    #[arg(short, long, default_value = "services/social-bot/config/config.toml")]
    pub config: PathBuf,

    /// Secrets TOML holding the Slack signing secret + Discord public key.
    /// No env-var fallback.
    #[arg(
        short = 's',
        long,
        default_value = "services/social-bot/config/secrets.toml"
    )]
    pub secrets: PathBuf,
}

cli_spec::define_program! {
    id          = "social-bot",
    cargo_pkg   = "social-bot",
    working_dir = ".",
    description = "Slack + Discord slash-command bots (one deployment). Allow-listed users \
                   post tweets from any twitter-service account via /tweet.",
    cli         = crate::Cli,
}
