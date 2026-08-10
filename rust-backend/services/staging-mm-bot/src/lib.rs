//! Library surface for the `staging-mm-bot` binary.
//!
//! A staging-only maker bot for the hybrid exchange: mints test tokens from
//! the permissionless faucet into its own `BalanceManager` escrow and quotes
//! tick-snapped bid/ask ladders around the oracle-service mid on every
//! `{SYM}/TUSDC` market the orderbook serves. Pure decisions live in
//! [`ladder`]; the async loops live in `main.rs`.

use std::path::PathBuf;

use clap::Parser;

pub mod client;
pub mod ladder;
pub mod server;
pub mod signing;

#[derive(Parser, Debug)]
#[command(name = "staging-mm-bot", about = "Faucet-funded maker bot for the hybrid exchange (staging)")]
pub struct Cli {
    #[arg(short, long, default_value = "services/staging-mm-bot/config/config.toml")]
    pub config: PathBuf,

    /// Per-binary secrets TOML. Holds the Sui signing key under the network
    /// selected by `network` in the bot config. The same key owns the
    /// BalanceManager, signs orders, and pays gas.
    #[arg(short = 's', long, default_value = "services/staging-mm-bot/config/secrets.toml")]
    pub secrets: PathBuf,

    /// Base URL of the orderbook service (markets, order intake, balances).
    #[arg(long, env = "ORDERBOOK_URL", default_value = "http://127.0.0.1:9014")]
    pub orderbook_url: String,

    /// Base URL of the token-info service. Source of the token catalog,
    /// decimals, Pyth feed ids, and the test-token faucet ids.
    #[arg(long, env = "TOKEN_INFO_URL", default_value = "http://127.0.0.1:9005")]
    pub token_info_url: String,

    /// Base URL of the oracle-service: live prices over its WS fanout.
    #[arg(long, env = "ORACLE_URL", default_value = "http://127.0.0.1:9013")]
    pub oracle_url: String,

    #[arg(long, default_value_t = 200_000_000)]
    pub gas_budget: u64,
}

cli_spec::define_program! {
    id          = "staging-mm-bot",
    cargo_pkg   = "staging-mm-bot",
    working_dir = ".",
    description = "Staging maker bot for the hybrid exchange. Mints test tokens from the \
                   faucet into a BalanceManager escrow and quotes oracle-tracking bid/ask \
                   ladders on every orderbook market.",
    cli         = crate::Cli,
}
