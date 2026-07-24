//! market-sim.
//!
//! Testnet DeepBook spot-liquidity simulator (SO-302): keeps a liquid
//! two-sided book on a configured set of spot pairs so the venue looks
//! and moves like a real market. Revived from mm-bot's pre-SO-299 spot
//! bands, now standalone — no options flows live here (mm-bot's sim
//! keeps the RFQ auction counterparty).
//!
//! Per pair, every `spot_interval_secs`: faucet-mint both sides, cancel
//! all resting orders, re-quote a bid/ask band around the Pyth cross.
//! Pools are created lazily (vendored-DEEP fee from the service wallet).
//!
//! Serves only `/health` + Prometheus `/metrics` (observability ops
//! server). Failures log at `warn` — the simulator must never page.

pub mod config;
pub mod liquidity;
pub mod sim;

pub use config::Config;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "market-sim",
    about = "Testnet DeepBook spot-liquidity simulator: faucet-funded bid/ask bands around the Pyth cross."
)]
pub struct Cli {
    #[arg(short, long, default_value = "services/market-sim/config/config.toml")]
    pub config: PathBuf,

    /// Base URL of the token-info service — token catalog (symbols, coin
    /// types, decimals, feeds, faucets) + DeepBook deployment handles.
    #[arg(long, env = "TOKEN_INFO_URL", default_value = "http://127.0.0.1:9005")]
    pub token_info_url: String,

    /// Base URL of the oracle-service: live prices over its WS fanout (the
    /// single Pyth gateway).
    #[arg(long, env = "ORACLE_URL", default_value = "http://127.0.0.1:9013")]
    pub oracle_url: String,

    /// Per-binary secrets TOML. Holds the Sui signing key under the network
    /// selected by `network` in the config. No env-var fallback.
    #[arg(short = 's', long, default_value = "services/market-sim/config/secrets.toml")]
    pub secrets: PathBuf,
}

cli_spec::define_program! {
    id          = "market-sim",
    cargo_pkg   = "market-sim",
    working_dir = ".",
    description = "Testnet DeepBook spot-liquidity simulator. Quotes faucet-funded bid/ask \
                   bands around the Pyth cross on configured spot pairs.",
    cli         = crate::Cli,
}
