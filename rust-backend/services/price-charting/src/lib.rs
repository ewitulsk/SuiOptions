//! Library surface for the `price-charting` binary (SO-156).
//!
//! OHLC charting for DeepBook option-bucket pools: a watcher tails fills
//! into TimescaleDB, an axum API serves carry-forward bars over REST and
//! pushes live bar updates over WS. The frontend's Lightweight Charts panel
//! (SO-157) is the consumer.

use std::path::PathBuf;

use clap::Parser;

pub mod apy;
pub mod apy_sampler;
pub mod bars;
pub mod config;
pub mod db;
pub mod mid_sampler;
pub mod router;
pub mod state;
pub mod watcher;

#[derive(Parser, Debug)]
#[command(name = "price-charting", about = "OHLC bars for DeepBook option pools")]
pub struct Cli {
    #[arg(short, long, default_value = "services/price-charting/config/config.toml")]
    pub config: PathBuf,

    /// Optional secrets TOML. Only `[sui] rpc_url` is read — the shared Sui
    /// RPC override rendered by render-secrets.sh. Optional: a missing or
    /// unreadable file falls back to the config / public RPC.
    #[arg(long)]
    pub secrets: Option<PathBuf>,
}

cli_spec::define_program! {
    id          = "price-charting",
    cargo_pkg   = "price-charting",
    working_dir = ".",
    description = "Watches DeepBook fills on tradeable bucket pools, stores trades in \
                   TimescaleDB, and serves OHLC bars over REST + WS for the Buy-screen chart.",
    cli         = crate::Cli,
}
