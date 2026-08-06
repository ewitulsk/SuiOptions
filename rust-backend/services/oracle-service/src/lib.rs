//! oracle-service (SO-254).
//!
//! The single internal holder of the one Pyth Hermes SSE subscription plus all
//! Pyth caching. It:
//!   - subscribes once to Hermes (live prices → `PriceCache`),
//!   - re-broadcasts every price update over an internal WS fanout (`/ws`),
//!   - serves the latest prices over REST (`/prices`, `/prices/:feed`,
//!     `/snapshot`),
//!   - serves cached + paced realized vol from Benchmarks (`/vol/realized`,
//!     shared `BenchmarkVol` across all callers).
//!
//! Every other service reads through `oracle-client` instead of talking to Pyth
//! directly, so the one external connection and the Pyth API key live here. The
//! keeper keeps a separate direct Hermes path only for the on-chain VAA.
//!
//! Since SO-353 the live-price SOURCE follows `[oracle] provider`: Hermes SSE
//! on pyth, a crossbar `/v2/simulate` poller on switchboard (`data_plane`) —
//! same cache, same fanout, same pyth-id keys either way.

pub mod config;
pub mod data_plane;
pub mod fanout;
pub mod router;
pub mod state;

pub use config::Config;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "oracle-service",
    about = "Single internal Pyth gateway: one SSE subscription, REST + WS fanout, cached realized vol."
)]
pub struct Cli {
    #[arg(short, long, default_value = "services/oracle-service/config/config.toml")]
    pub config: PathBuf,

    /// Rendered secrets TOML carrying the Pyth API key (`[pyth] api_key`).
    #[arg(
        short = 's',
        long,
        default_value = "services/oracle-service/config/secrets.toml"
    )]
    pub secrets: PathBuf,
}

cli_spec::define_program! {
    id          = "oracle-service",
    cargo_pkg   = "oracle-service",
    working_dir = ".",
    description = "Single internal Pyth gateway: one SSE subscription, REST + WS fanout, cached realized vol.",
    cli         = crate::Cli,
}
