//! Quoting service (§5).
//!
//! Pure stateful WS router between retail clients and market makers — holds
//! no funds, signs no transactions. The on-chain protocol is the safety net:
//! oversubscribed MMs revert at execution, the reputation system catches up
//! over many such reverts.
//!
//! Internal shape:
//!
//! - [`state`] owns everything mutable — Account mirrors (balances minus
//!   active reservations), buckets, the live reservation table, MM
//!   reputation.
//! - [`rfq`] orchestrates one RFQ end to end: broadcast to MMs, collect with
//!   a deadline, validate, reserve, sort, ship to retail.
//! - [`ws`] is the transport. It owns no state — every interesting decision
//!   happens in [`state`] or [`rfq`].
//! - [`indexer_client`] subscribes to the indexer's stream and pipes events
//!   into [`state`].

pub mod config;
pub mod errors;
pub mod indexer_client;
pub mod rfq;
pub mod state;
pub mod ws;

pub use config::Config;
pub use errors::ServiceError;
pub use state::AppState;
