//! dakota-service.
//!
//! Fronts the [Dakota](https://docs.dakota.xyz) stablecoin on/off-ramp platform
//! for our admin, partner-business and individual-customer dashboards.
//!
//! ## Hierarchy
//!
//! Dakota's object model carries the three tiers directly:
//!
//! ```text
//! us (Client) ─┬─ partner business  (Customer, is_sub_client: true)
//!              │     └─ its customers (Customer, sub_client_id: <business>)
//!              └─ our own customers  (Customer, no sub_client_id)
//! ```
//!
//! A JWT's `role` + `scope` decides which slice of that a caller sees; see
//! [`authz`]. Scope is read only from the verified token, never from a request.
//!
//! ## No PII
//!
//! Dakota responses are full of it — `GET /customers` returns `email` and
//! `name`, `POST /accounts` returns `bank_account.account_holder_name` and
//! `account_number`, `GET /events` returns `sender_details`. **None of it is
//! persisted.** We store Dakota KSUIDs, enums, amounts, assets and timestamps;
//! anything identifying is fetched per-request and relayed straight to the
//! browser. Handlers that need to show a name return `serde_json::Value`
//! rather than binding a struct, so there is nothing to accidentally write.
//!
//! Onboarding follows from that: customers are handed to Dakota's hosted
//! `application_url`, and beneficial owners, documents and SSNs never touch
//! this code.
//!
//! ## Staging only
//!
//! This service is declared in `docker-compose.staging.yml` and deliberately
//! absent from the prod compose file, which is what keeps `deploy.sh` from ever
//! planning it into prod.

pub mod authz;
pub mod config;
pub mod dakota;
pub mod db;
pub mod handlers;
pub mod invites;
pub mod router;
pub mod state;
pub mod wallet;
pub mod webhook;

pub use config::Config;
pub use state::AppState;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "dakota-service",
    about = "Dakota on/off-ramp integration: customers, ramps, treasury and flow tracking."
)]
pub struct Cli {
    #[arg(short, long, default_value = "services/dakota-service/config/config.toml")]
    pub config: PathBuf,

    /// Secrets TOML holding `dakota.api_key`. No env-var fallback.
    #[arg(
        short = 's',
        long,
        default_value = "services/dakota-service/config/secrets.toml"
    )]
    pub secrets: PathBuf,
}

cli_spec::define_program! {
    id          = "dakota-service",
    cargo_pkg   = "dakota-service",
    working_dir = ".",
    description = "Dakota stablecoin on/off-ramp integration. Hosted-redirect onboarding, \
                   onramp/offramp/swap accounts, Ed25519-verified webhooks and a PII-free \
                   activity ledger for admin, partner-business and individual dashboards.",
    cli         = crate::Cli,
}
