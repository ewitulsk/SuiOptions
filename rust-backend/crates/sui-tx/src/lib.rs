//! Sui chain-side glue: RPC client, signer, PTB builders, scheme-aware
//! quote signer, and the workspace's thin WebSocket helpers.
//!
//! Everything that talks to the chain (or signs payloads destined for the
//! chain) lives here. Off-chain pure-data types live in `protocol-types`;
//! the `deployments.json` catalog lives in `deployments`. This crate
//! depends on both but neither depends on it.

pub mod chain;
pub mod events;
pub mod quote_signer;
pub mod sui_client;
pub mod tx;
pub mod ws_client;

pub use chain::{ChainClient, CoinRef, ExecutedTransaction};
pub use events::EventClient;
pub use quote_signer::QuoteSigner;
pub use sui_client::{Network, Signer, SuiClientWrapper};
