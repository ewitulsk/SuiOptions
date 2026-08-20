//! Library surface for the `exchange` binary.
//!
//! Hosts the clap [`Cli`] / [`Command`] enum so the control-panel TUI can
//! introspect every subcommand and its flags, plus the bucket-create
//! executor (moved here from the option-scheduler when bucket rolling was
//! removed from that service). Business logic lives in `main.rs`.

pub mod roller;
pub mod strike_grid;

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use sui_types::base_types::{ObjectID, SuiAddress};

use sui_tx::sui_client::Network;
use sui_tx::tx::admin::WhitelistDomain;

/// clap value parser for `--domain` (anyhow errors aren't clap-compatible).
fn parse_domain(s: &str) -> Result<WhitelistDomain, String> {
    s.parse().map_err(|e: anyhow::Error| e.to_string())
}

/// Option product selector for bucket creation. Mirrors
/// [`roller::ProductType`]; `call` is the default so existing behaviour is
/// unchanged.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Product {
    Call,
    Put,
}

#[derive(Parser, Debug)]
#[command(name = "exchange", about = "Admin CLI for the covered-call options protocol")]
pub struct Cli {
    /// Base URL of the token-info service. Source of truth for the token
    /// catalog and on-chain ids. No `deployments.json` fallback — a hard
    /// cutover; the tool crashes if token-info is unreachable.
    #[arg(long, env = "TOKEN_INFO_URL", default_value = "http://127.0.0.1:9005")]
    pub token_info_url: String,

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
        /// `call` (covered call) or `put` (cash-secured put). Defaults to
        /// `call` so existing invocations are unchanged.
        #[arg(long, value_enum, default_value_t = Product::Call)]
        product: Product,
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
    /// Mint a test token and deposit it into an MM's own
    /// `mm_collateral::CollateralAccount` in one PTB (core holds no MM funds
    /// under the collateral abstraction). Use for fast-MM-bootstrap or any
    /// "give this collateral account some settlement asset to quote with"
    /// workflow.
    FundAccount {
        /// The shared CollateralAccount object id.
        #[arg(long)]
        account: ObjectID,
        /// The MM's published mm_collateral package id.
        #[arg(long)]
        collateral_package: ObjectID,
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
    /// Add an address to the ingress whitelist in one PTB. `--domain` is
    /// repeatable (options, exchange, vault-create, vault-lp); omitting it
    /// targets ALL four domains.
    WhitelistAdd {
        #[arg(long)]
        address: SuiAddress,
        #[arg(long = "domain", value_parser = parse_domain)]
        domains: Vec<WhitelistDomain>,
    },
    /// Remove an address from the ingress whitelist in one PTB. `--domain`
    /// is repeatable; omitting it targets ALL four domains.
    WhitelistRemove {
        #[arg(long)]
        address: SuiAddress,
        #[arg(long = "domain", value_parser = parse_domain)]
        domains: Vec<WhitelistDomain>,
    },
    /// Print the whitelist object's per-domain members + enabled/paused
    /// flags.
    WhitelistList,
    /// Print the domains an address is currently whitelisted on.
    WhitelistDomains {
        #[arg(long)]
        address: SuiAddress,
    },
    /// Turn the member check ON (guarded-launch mode). `--domain` is
    /// repeatable; omitting it targets ALL four domains.
    WhitelistEnable {
        #[arg(long = "domain", value_parser = parse_domain)]
        domains: Vec<WhitelistDomain>,
    },
    /// Turn the member check OFF — the go-public lever. Membership is
    /// retained on-chain; re-enabling restores the cohort. `--domain` is
    /// repeatable; omitting it targets ALL four domains.
    WhitelistDisable {
        #[arg(long = "domain", value_parser = parse_domain)]
        domains: Vec<WhitelistDomain>,
    },
    /// Big red button, one PTB: pause all ingress (every whitelist
    /// domain), the trading-vault registry, and every exchange market
    /// registry. Exits (withdrawals/cancels) are never gated.
    PauseIngress,
    /// Reverse of `pause-ingress`, one PTB.
    UnpauseIngress,
    /// SO-416 backfill: list an exchange market for every live bucket that
    /// doesn't have one yet, via the permissionless exchange-listing
    /// entries. Safe to re-run — already-listed buckets are skipped
    /// (locally when the api reports a market, on-chain by the dedup
    /// abort otherwise).
    ListMarkets {
        /// api-service base URL to enumerate buckets from.
        #[arg(long, default_value = "http://127.0.0.1:9003")]
        api_url: String,
        /// Print the plan without submitting.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
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
