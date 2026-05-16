//! Test clients for the options protocol.
//!
//! Three binaries live in `bin/`:
//!
//! - **`exchange`** — admin/operator CLI. Creates buckets, sets fees, withdraws
//!   from treasury. Signs as the deployer.
//! - **`writer`** — retail-writer frontend stand-in. Requests an RFQ from the
//!   quoting service and submits the resulting `execute_write` PTB.
//! - **`mm-bot`** — basic market-maker bot. Auto-bootstraps an Account on
//!   first run, connects to the quoting service as a Trader MM, prices each
//!   incoming RFQ with Black-Scholes, signs and returns a `Quote`.
//!
//! Shared modules:
//!
//! - [`deployments`] — loader for `deployments.json` (package id, AdminCap,
//!   ProtocolConfig, Treasury) plus the derived on-chain `protocol_id` bytes.
//! - [`sui_client`] — thin wrapper over `sui_sdk::SuiClient` + a `Signer`
//!   loaded from env vars.
//! - [`tx`] — PTB builders, one per protocol entrypoint.
//! - [`ws_client`] — minimal WS client for the quoting service.
//! - [`pricing`] — Black-Scholes call valuation, used by the MM bot.

pub mod deployments;
pub mod pricing;
pub mod sui_client;
pub mod tx;
pub mod ws_client;

pub use deployments::{Deployments, NetworkDeployment};
pub use sui_client::{Network, Signer, SuiClientWrapper};
