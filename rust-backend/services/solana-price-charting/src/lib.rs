//! Library surface for the `solana-price-charting` binary.
//!
//! Port of `services/price-charting` **without any order-book ingestion**
//! (no Solana order-book integration exists yet). Ships the Timescale
//! storage, candle aggregation, REST/WS serving, and the vault-APY sampler.
//! The trade/mid tables stay empty until an ingestion source lands: `/pools`
//! returns `[]`, `/bars` returns empty arrays, `/ws` accepts subscriptions
//! and pushes nothing — exactly what empty tables produce, no special-casing.
//! When a Solana venue integration lands it adds an ingestion task that
//! writes `pool_trades`/`pool_mids` and broadcasts on `AppState`; everything
//! downstream already works.

use std::path::PathBuf;

use clap::Parser;

pub mod apy;
pub mod apy_sampler;
pub mod bars;
pub mod config;
pub mod db;
pub mod router;
pub mod state;

#[derive(Parser, Debug)]
#[command(
    name = "solana-price-charting",
    about = "OHLC bar storage/serving + vault-APY sampler for Solana options"
)]
pub struct Cli {
    #[arg(
        short,
        long,
        default_value = "services/solana-price-charting/config/config.toml"
    )]
    pub config: PathBuf,
}

cli_spec::define_program! {
    id          = "solana-price-charting",
    cargo_pkg   = "solana-price-charting",
    working_dir = ".",
    description = "Serves OHLC bars over REST + WS (empty until a Solana order-book \
                   ingestion source lands) and samples covered-call vault APY \
                   (predicted + realized) into TimescaleDB.",
    cli         = crate::Cli,
}
