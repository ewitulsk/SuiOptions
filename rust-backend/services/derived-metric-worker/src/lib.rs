//! derived-metric-worker.
//!
//! A periodic worker that owns its own Postgres DB and computes **predicted
//! APY** for every covered-call vault, on a timer. Realized APY is served by
//! the indexer (a SQL view over chain data); this service computes the
//! forward-looking line the indexer can't, because it needs market data
//! (Pyth spot + realized vol) and an options-pricing model that don't belong
//! in the indexer's separation of concerns.
//!
//! Each tick, per vault:
//!   - **Tier 1 (current round)** — annualize the premium the vault is on
//!     track to collect this round, from its live on-chain RFQ auctions.
//!   - **Tier 2 (forecast)** — Black–Scholes premium at the keeper's
//!     0.10-delta target strike, net of fees, annualized, for the next K
//!     rounds.
//! Results are upserted into `vault_predictions` and served over a small read
//! API (`GET /vault-apy/:vault_id`) that api-service composes with the
//! indexer's realized series.
//!
//! Observability (SO-180): `observability::init` gives JSON logs → Loki,
//! Prometheus `/metrics`, and OTLP traces → Tempo. Failures worth paging are
//! emitted as `error!(alert_id = "…", …)` — the generic Grafana rule fires on
//! any non-empty `alert_id`, no per-alert infra.

pub mod compute;
pub mod config;
pub mod db;
pub mod read_api;
pub mod spot;

pub use config::Config;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "derived-metric-worker",
    about = "Computes predicted-APY snapshots for covered-call vaults; owns its DB; serves a read API."
)]
pub struct Cli {
    #[arg(
        short,
        long,
        default_value = "services/derived-metric-worker/config/config.toml"
    )]
    pub config: PathBuf,
}

cli_spec::define_program! {
    id          = "derived-metric-worker",
    cargo_pkg   = "derived-metric-worker",
    working_dir = ".",
    description = "Computes predicted-APY snapshots for covered-call vaults (Pyth spot + vol + Black–Scholes); owns its Postgres DB.",
    cli         = crate::Cli,
}
