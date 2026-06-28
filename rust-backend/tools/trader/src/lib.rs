//! Library surface for the `trader` binary.
//!
//! Hosts the clap [`Cli`] type and the [`program_spec`] entry point so the
//! control-panel TUI can introspect the binary's flags without exec'ing it.
//! The actual trader flow lives in `main.rs`.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use sui_types::base_types::ObjectID;

use sui_tx::sui_client::Network;

/// Option product the trader flow targets. `call` is the default so existing
/// invocations are unchanged.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Product {
    Call,
    Put,
}

#[derive(Parser, Debug)]
#[command(name = "trader", about = "Retail-trader test client for the options protocol")]
pub struct Cli {
    #[arg(long, env = "TOKEN_INFO_URL", default_value = "http://127.0.0.1:9005")]
    pub token_info_url: String,

    /// Per-binary secrets TOML. Holds the Sui signing key. No env-var
    /// fallback.
    #[arg(short = 's', long, default_value = "tools/trader/config/secrets.toml")]
    pub secrets: PathBuf,

    #[arg(short, long, value_enum, default_value_t = Network::Testnet)]
    pub network: Network,

    #[arg(short = 'q', long, default_value = "ws://127.0.0.1:9002/")]
    pub quoting_url: String,

    /// Bucket id we're buying a call from.
    #[arg(short, long)]
    pub bucket: ObjectID,

    /// Call amount we're buying, in raw underlying smallest-units (see token
    /// decimals in the token-info service's testTokens).
    #[arg(short = 'w', long)]
    pub write_amount: u64,

    /// Symbol for the underlying token (TBTC, TDEEP, TUSDC, TWAL).
    #[arg(long, default_value = "TBTC")]
    pub underlying: String,

    /// Symbol for the settlement token (the premium is paid in this).
    #[arg(long, default_value = "TUSDC")]
    pub settlement: String,

    /// `call` (covered call) or `put` (cash-secured put). Defaults to `call`.
    #[arg(long, value_enum, default_value_t = Product::Call)]
    pub product: Product,

    #[arg(long, default_value_t = 200_000_000)]
    pub gas_budget: u64,

    #[arg(long, default_value_t = 5)]
    pub rfq_timeout_secs: u64,
}

cli_spec::define_program! {
    id          = "trader",
    cargo_pkg   = "trader",
    working_dir = ".",
    description = "Retail-trader test client. Walks the §7.2 trader flow end to end: RFQ \
                   over WS (side=Trader), pick the best (lowest-premium) ask, submit one PTB \
                   that mints the settlement premium via the faucet and calls \
                   bucket::execute_write with FlowKind::Trader.",
    cli         = crate::Cli,
}
