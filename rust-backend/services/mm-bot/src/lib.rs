//! Library surface for the `mm-bot` binary.
//!
//! Hosts the clap [`Cli`] type, the [`program_spec`] entry point, and the
//! [`pricing`] module that captures the pure parts of the market-making
//! process. The async bot loop lives in `main.rs`.

use std::path::PathBuf;

use clap::Parser;

pub mod pricing;

#[derive(Parser, Debug)]
#[command(name = "mm-bot", about = "Test market-maker bot for the options protocol")]
pub struct Cli {
    #[arg(short, long, default_value = "services/mm-bot/config/config.toml")]
    pub config: PathBuf,

    #[arg(long, default_value = "services/mm-bot/config/mm-bot.account.json")]
    pub account_state: PathBuf,

    #[arg(short, long, default_value = "deployments.json")]
    pub deployments: PathBuf,

    /// Per-binary secrets TOML. Holds the Sui signing key (under the
    /// network selected by `network` in the bot config) and the
    /// quote-signing key (`mm_bot.quote_key`). No env-var fallback.
    #[arg(short = 's', long, default_value = "services/mm-bot/config/secrets.toml")]
    pub secrets: PathBuf,

    #[arg(long, default_value_t = 200_000_000)]
    pub gas_budget: u64,
}

cli_spec::define_program! {
    id          = "mm-bot",
    cargo_pkg   = "mm-bot",
    working_dir = ".",
    description = "Market-maker bot. First run bootstraps a shared Account and funds it with \
                   settlement via the faucet; every run authenticates over WS and prices \
                   incoming RFQs with Black-Scholes.",
    cli         = crate::Cli,
}
