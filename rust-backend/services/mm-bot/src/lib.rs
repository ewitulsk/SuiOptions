//! Library surface for the `mm-bot` binary.
//!
//! Hosts the clap [`Cli`] type, the [`program_spec`] entry point, and the
//! [`pricing`] module that captures the pure parts of the market-making
//! process. The async bot loop lives in `main.rs`.

use std::path::PathBuf;

use clap::Parser;

pub mod deepbook;
pub mod liquidity;
pub mod onchain_rfq;
pub mod onchain_swap;
pub mod pricing;

#[derive(Parser, Debug)]
#[command(name = "mm-bot", about = "Test market-maker bot for the options protocol")]
pub struct Cli {
    #[arg(short, long, default_value = "services/mm-bot/config/config.toml")]
    pub config: PathBuf,

    /// Base URL of the token-info service. Resolved at boot via
    /// `token-info-client`; hard cutover — no deployments.json fallback.
    #[arg(long, env = "TOKEN_INFO_URL", default_value = "http://127.0.0.1:9005")]
    pub token_info_url: String,

    /// Base URL of the api-service. The bot resolves each RFQ's bucket
    /// (strike, expiry, coin types) from here by address, so it never trusts
    /// pricing inputs delivered on the RFQ broadcast itself.
    #[arg(long, env = "API_URL", default_value = "http://127.0.0.1:9003")]
    pub api_url: String,

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
