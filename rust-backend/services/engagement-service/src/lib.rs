//! engagement-service.
//!
//! Tracks engagement on tweets that mention our account and converts it into
//! airdrop points (MVP: one service owns tracking, point conversion and the
//! leaderboard; the conversion lives in its own module so it can be split
//! out later if the airdrop program outgrows this).
//!
//! A poll loop asks twitter-service for new mentions of the configured
//! account and for refreshed engagement counters on tweets it already knows,
//! and persists both in Postgres. Points are derived at read time from
//! config weights, so re-tuning weights never needs a backfill.
//!
//! Internal-only: the bind port is reachable on the compose `net` network
//! (e.g. by airdrop-bot) and is deliberately never proxied by nginx.
//!
//! Endpoints:
//! - `GET /health`
//! - `GET /leaderboard?limit=N` — authors ranked by airdrop points.
//! - `GET /points/{handle}` — one author's totals, points and rank.

pub mod config;
pub mod db;
pub mod handlers;
pub mod points;
pub mod poller;
pub mod router;
pub mod state;
pub mod twitter_client;

pub use config::Config;
pub use state::AppState;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "engagement-service",
    about = "Tracks tweet engagement for mentions of our account and serves the airdrop leaderboard."
)]
pub struct Cli {
    #[arg(
        short,
        long,
        default_value = "services/engagement-service/config/config.toml"
    )]
    pub config: PathBuf,
}

cli_spec::define_program! {
    id          = "engagement-service",
    cargo_pkg   = "engagement-service",
    working_dir = ".",
    description = "Engagement + airdrop-points service. Polls twitter-service for mentions of \
                   our account, persists per-tweet engagement counters, converts them into \
                   airdrop points and serves the leaderboard.",
    cli         = crate::Cli,
}
