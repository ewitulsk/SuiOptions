//! airdrop-bot.
//!
//! Discord slash-command bot for the engagement airdrop. Deliberately a
//! SEPARATE Discord application + deployment from social-bot (the tweeting
//! bot): different audience (community server vs team), different blast
//! radius, and no shared secrets.
//!
//! Commands are read-only, so there is no allow list — anyone in a server
//! the app is installed in can query:
//!
//!   /leaderboard [count]   — top authors by airdrop points
//!   /points <handle>       — one twitter handle's points + rank
//!
//! Discord delivers commands as signed HTTP webhooks (no gateway
//! connection), proxied by nginx at /<env>/airdrop-bot/:
//! - `POST /discord/interactions` — Ed25519-verified interactions endpoint.
//! - `GET /health`

pub mod commands;
pub mod config;
pub mod discord;
pub mod engagement_client;
pub mod router;
pub mod secrets;
pub mod state;

pub use config::Config;
pub use secrets::BotSecrets;
pub use state::AppState;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "airdrop-bot",
    about = "Discord bot: airdrop leaderboard + per-handle points from engagement-service."
)]
pub struct Cli {
    #[arg(short, long, default_value = "services/airdrop-bot/config/config.toml")]
    pub config: PathBuf,

    /// Secrets TOML holding the Discord application public key.
    /// No env-var fallback.
    #[arg(
        short = 's',
        long,
        default_value = "services/airdrop-bot/config/secrets.toml"
    )]
    pub secrets: PathBuf,
}

cli_spec::define_program! {
    id          = "airdrop-bot",
    cargo_pkg   = "airdrop-bot",
    working_dir = ".",
    description = "Discord slash-command bot for the engagement airdrop: /leaderboard and \
                   /points, served from engagement-service. Separate Discord application \
                   from social-bot.",
    cli         = crate::Cli,
}
