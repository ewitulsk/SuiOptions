//! Library surface for the solana-keeper binary — the Solana port of
//! `services/keeper` (guide doc 09).
//!
//! The keeper is the permissionless crank-driver for the covered-call
//! vaults: every tick it reads each discovered vault's chain state,
//! decides the one action the round needs next ([`planner`]), and submits
//! it — with fresh Pyth `PriceUpdateV2` posts ahead of the oracle-gated
//! cranks ([`pyth_leg`]). It holds only a funded gas wallet —
//! `options_vault` validates everything that matters.

use std::path::PathBuf;

use clap::Parser;
use solana_tx::Network;

pub mod config;
pub mod discovery;
pub mod planner;
pub mod pyth_leg;
pub mod slicing;
pub mod state;
pub mod strike;
pub mod submit;

#[derive(Parser, Debug)]
#[command(
    name = "solana-keeper",
    about = "Permissionless covered-call vault crank (Solana): redeems positions, selects \
             buckets, opens/settles RFQ slices, swaps proceeds, and finalizes rounds."
)]
pub struct Cli {
    #[arg(short, long, default_value = "config/config.toml")]
    pub config: PathBuf,

    /// Base URL of solana-token-info: program-id registry + the
    /// supported-token catalog. Hard cutover: the keeper crashes at boot
    /// if it never comes up.
    #[arg(long, env = "TOKEN_INFO_URL", default_value = "http://127.0.0.1:9005")]
    pub token_info_url: String,

    /// Base URL of solana-oracle-service: spot prices + realized vol (the
    /// single Pyth read gateway). The keeper still hits Hermes directly
    /// for on-chain update data, which a price cache can't serve.
    #[arg(long, env = "ORACLE_URL", default_value = "http://127.0.0.1:9013")]
    pub oracle_url: String,

    /// Per-binary secrets TOML holding the Solana gas keypair. Any funded
    /// wallet works — the keeper holds no privileged accounts.
    #[arg(short = 's', long, default_value = "config/secrets.toml")]
    pub secrets: PathBuf,

    #[arg(short, long, value_enum, default_value_t = Network::Devnet)]
    pub network: Network,

    /// Full planning every tick, log the intents, submit nothing.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}
