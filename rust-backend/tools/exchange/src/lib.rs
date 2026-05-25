//! Library surface for the `exchange` binary.
//!
//! Hosts the clap [`Cli`] / [`Command`] enum so the control-panel TUI can
//! introspect every subcommand and its flags. Business logic lives in
//! `main.rs`.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use sui_types::base_types::{ObjectID, SuiAddress};

use sui_tx::sui_client::Network;

#[derive(Parser, Debug)]
#[command(name = "exchange", about = "Admin CLI for the covered-call options protocol")]
pub struct Cli {
    #[arg(short, long, default_value = "deployments.json")]
    pub deployments: PathBuf,

    /// Per-binary secrets TOML. Holds the Sui signing key. No env-var
    /// fallback.
    #[arg(short = 's', long, default_value = "tools/exchange/config/secrets.toml")]
    pub secrets: PathBuf,

    #[arg(short, long, value_enum, default_value_t = Network::Testnet)]
    pub network: Network,

    #[arg(long, default_value_t = 200_000_000)]
    pub gas_budget: u64,

    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// `bucket::new_call_option<U, S>` — creates `count` buckets at
    /// `start_strike + i * strike_interval` for `i ∈ [0, count)`. Real
    /// strike ratio is `strike / 10^strike_scale` (see SO-55). Tokens
    /// are looked up by symbol from `deployments.testTokens`.
    CreateBuckets {
        #[arg(long, default_value = "TBTC")]
        underlying: String,
        #[arg(long, default_value = "TUSDC")]
        settlement: String,
        #[arg(long)]
        expiry_ms: u64,
        #[arg(long)]
        start_strike: u128,
        #[arg(long)]
        strike_interval: u128,
        #[arg(long)]
        count: u64,
        /// 0..=9. Scheduler auto-derives this; admin uses it manually.
        #[arg(long, default_value_t = 0)]
        strike_scale: u8,
    },
    /// Faucet-mint `amount` of a test token to the signer.
    Mint {
        #[arg(long)]
        token: String,
        #[arg(long)]
        amount: u64,
    },
    /// Mint a test token and deposit it into the given Account in one PTB.
    /// Use for fast-MM-bootstrap or any "give Account X some settlement
    /// asset to quote with" workflow.
    FundAccount {
        #[arg(long)]
        account: ObjectID,
        #[arg(long)]
        token: String,
        #[arg(long)]
        amount: u64,
    },
    /// `admin::set_fee_bps`.
    SetFee {
        #[arg(long)]
        bps: u64,
    },
    /// `treasury::withdraw<T>`. `--token` accepts a symbol from
    /// `deployments.testTokens` or any fully-qualified Move type.
    WithdrawTreasury {
        #[arg(long)]
        token: String,
        #[arg(long)]
        amount: u64,
        #[arg(long)]
        recipient: SuiAddress,
    },
    /// Print every id resolvable from `deployments.json`.
    Info,
}

cli_spec::define_program! {
    id          = "exchange",
    cargo_pkg   = "exchange",
    working_dir = ".",
    description = "Admin / operator CLI. Drives every AdminCap-gated entrypoint plus the \
                   test-token faucets — create buckets, mint, fund accounts, set fees, \
                   withdraw treasury.",
    cli         = crate::Cli,
}
