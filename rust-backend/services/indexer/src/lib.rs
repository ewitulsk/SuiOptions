//! Indexer (§6).
//!
//! Tails Sui's checkpoint stream via [`sui_data_ingestion_core`], BCS-decodes
//! events emitted by the `options_protocol` Move package, ingests them into
//! [`store::Store`], and fans the live stream out to the quoting service
//! over WS ([`fanout::serve`]).
//!
//! - [`worker::ProtocolEventWorker`] implements the framework's `Worker`
//!   trait. Pure dispatch lives in [`event_types::dispatch`] so the BCS path
//!   is unit-testable without spinning up the framework.
//! - [`store::Store`] is the in-memory event log + materialized views
//!   (accounts, buckets, positions) + tokio broadcast channel.
//! - [`fanout`] serves snapshots from the log and live events from the
//!   broadcast.

pub mod config;
pub mod event_types;
pub mod fanout;
pub mod store;
pub mod worker;

pub use config::Config;
pub use event_types::EventTypes;
pub use store::{AccountState, BucketState, PositionState, Store};
pub use worker::ProtocolEventWorker;
