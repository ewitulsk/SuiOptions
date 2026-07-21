//! Library surface for the `mm-bot` binary.
//!
//! Hosts the clap [`Cli`] type, the [`program_spec`] entry point, and the
//! [`pricing`] module that captures the pure parts of the market-making
//! process. The async bot loop lives in `main.rs`.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

pub mod collateral;
pub mod desk;
pub mod liquidity;
pub mod pricing;
pub mod sim;

/// Move abort code emitted when a bid is outbid between read and submit
/// (`auction::errors::bid_too_low` in the generic auction package, shared
/// by every venue). This is the expected lost-race outcome, so it stays a
/// `warn!` while every other bid failure fires the `tx-failed-*` alert.
const AUCTION_BID_TOO_LOW: u64 = 5;

/// Pull the abort code out of a revert message like
/// `… MoveAbort(MoveLocation { … }, 31) in command 2` (mirrors the keeper's
/// triage parser: the location debug-print nests braces, so scan every
/// `}, ` for the one followed by `<digits>)`).
fn extract_abort_code(msg: &str) -> Option<u64> {
    let after = &msg[msg.find("MoveAbort(")? + "MoveAbort(".len()..];
    for (i, _) in after.match_indices("}, ") {
        let rest = &after[i + 3..];
        if let Some(end) = rest.find(')') {
            if let Ok(code) = rest[..end].trim().parse() {
                return Some(code);
            }
        }
    }
    None
}

/// True when a bid failed only because someone outbid us between read and
/// submit — the benign lost-race path that must not page. Requires the
/// abort to come from the `auction` module so an unrelated code-5 abort
/// (e.g. options_core `quote_bucket_mismatch`) still pages.
pub(crate) fn is_benign_bid_loss(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}");
    extract_abort_code(&msg) == Some(AUCTION_BID_TOO_LOW)
        && msg.contains("Identifier(\"auction\")")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn revert(module: &str, code: u64) -> anyhow::Error {
        anyhow::anyhow!(
            "auction::bid reverted: MoveAbort(MoveLocation {{ module: ModuleId {{ \
             address: abc, name: Identifier(\"{module}\") }}, function: 9, instruction: 12, \
             function_name: Some(\"bid\") }}, {code}) in command 3"
        )
    }

    #[test]
    fn bid_too_low_is_benign() {
        assert!(is_benign_bid_loss(&revert("auction", AUCTION_BID_TOO_LOW)));
    }

    #[test]
    fn other_aborts_are_not_benign() {
        assert!(!is_benign_bid_loss(&revert("auction", 54)));
        // Code 5 from another module (e.g. options_core's
        // quote_bucket_mismatch) is NOT a lost bid race.
        assert!(!is_benign_bid_loss(&revert("quote", AUCTION_BID_TOO_LOW)));
        assert!(!is_benign_bid_loss(&anyhow::anyhow!("insufficient gas")));
    }
}

#[derive(Parser, Debug)]
#[command(name = "mm-bot", about = "Test market-maker bot for the options protocol")]
pub struct Cli {
    #[arg(short, long, default_value = "services/mm-bot/config/config.toml")]
    pub config: PathBuf,

    /// Base URL of the token-info service. Resolved at boot via
    /// `token-info-client`; hard cutover — no deployments.json fallback.
    #[arg(long, env = "TOKEN_INFO_URL", default_value = "http://127.0.0.1:9005")]
    pub token_info_url: String,

    /// Base URL of the oracle-service: live prices over its WS fanout (the
    /// single Pyth gateway). Replaces the bot's own Hermes subscription.
    #[arg(long, env = "ORACLE_URL", default_value = "http://127.0.0.1:9013")]
    pub oracle_url: String,

    /// Base URL of the api-service. The bot resolves each RFQ's bucket
    /// (strike, expiry, coin types) from here by address, so it never trusts
    /// pricing inputs delivered on the RFQ broadcast itself.
    #[arg(long, env = "API_URL", default_value = "http://127.0.0.1:9003")]
    pub api_url: String,

    /// Indexer GraphQL endpoint. The desk reconstructs its book (NAV,
    /// vault custody) from the trading-vault views here.
    #[arg(
        long,
        env = "INDEXER_GRAPHQL_URL",
        default_value = "http://127.0.0.1:9002/graphql"
    )]
    pub indexer_graphql_url: String,

    /// Per-binary secrets TOML. Holds the Sui signing key (under the
    /// network selected by `network` in the bot config) and the
    /// quote-signing key (`mm_bot.quote_key`). No env-var fallback.
    #[arg(short = 's', long, default_value = "services/mm-bot/config/secrets.toml")]
    pub secrets: PathBuf,

    #[arg(long, default_value_t = 200_000_000)]
    pub gas_budget: u64,

    /// Optional subcommand. Absent → run the bot (serve mode).
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Publish this MM's own copy of the `mm_collateral` package (compiled
    /// against the current deployment's published options_core) and persist
    /// `{package_id, account_id, upgrade_cap}` for serve mode. The package
    /// is copied to a temp dir before publishing so the repo tree — notably
    /// the template's `Published.toml` — is never mutated (each MM's publish
    /// is theirs alone).
    DeployCollateral {
        /// Path to the mm-collateral Move package template.
        #[arg(long, default_value = "../contracts/mm-collateral")]
        contracts: PathBuf,
        /// Where to persist the deployment record. Defaults to
        /// `services/mm-bot/config/collateral.<network>.toml`.
        #[arg(long)]
        out: Option<PathBuf>,
    },
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
