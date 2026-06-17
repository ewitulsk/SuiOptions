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

/// Move abort code emitted when a bid is outbid between read and submit
/// (`errors::rfq_bid_too_low`, shared by `rfq` and `swap_auction`). This is
/// the expected lost-race outcome, so it stays a `warn!` while every other
/// bid failure fires the `tx-failed-*` alert.
const RFQ_BID_TOO_LOW: u64 = 31;

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
/// submit — the benign lost-race path that must not page.
pub(crate) fn is_benign_bid_loss(err: &anyhow::Error) -> bool {
    extract_abort_code(&format!("{err:#}")) == Some(RFQ_BID_TOO_LOW)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn revert(code: u64) -> anyhow::Error {
        anyhow::anyhow!(
            "rfq::bid reverted: MoveAbort(MoveLocation {{ module: ModuleId {{ \
             address: abc, name: Identifier(\"rfq\") }}, function: 9, instruction: 12, \
             function_name: Some(\"bid\") }}, {code}) in command 3"
        )
    }

    #[test]
    fn bid_too_low_is_benign() {
        assert!(is_benign_bid_loss(&revert(RFQ_BID_TOO_LOW)));
    }

    #[test]
    fn other_aborts_are_not_benign() {
        assert!(!is_benign_bid_loss(&revert(54)));
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
