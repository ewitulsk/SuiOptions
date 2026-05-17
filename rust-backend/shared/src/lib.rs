//! Shared library for every service and tool in the workspace.
//!
//! Pulls together what used to live in `protocol-types`, `deployments`,
//! `secrets`, and the lib portion of the old `clients` crate. Each
//! sub-module retains its original public API; the convenience re-exports
//! at the crate root cover the types you'll touch most often.
//!
//! Sub-modules:
//!
//! - [`protocol_types`] — BCS-canonical Quote/SignedQuote + WS message
//!   envelopes + indexer event mirrors. The byte layout is the contract.
//! - [`deployments`] — loader for `deployments.json` (package, AdminCap,
//!   Treasury, test tokens, faucets).
//! - [`secrets`] — loader for per-binary `secrets.toml` (Sui keys, MM
//!   quote key). No env-var fallback by design.
//! - [`sui_client`] — `SuiClient` + `Signer` wrapper, loaded from a
//!   [`secrets::Secrets`].
//! - [`pricing`] — Black-Scholes call valuation for the MM bot.
//! - [`quote_signer`] — scheme-aware Ed25519/Secp256k1/Secp256r1 signer
//!   that accepts either a Sui bech32 keypair or a raw hex secret.
//! - [`ws_client`] — minimal WS helpers used by retail/MM clients.
//! - [`tx`] — PTB builders, one module per protocol entry point.

pub mod deployments;
pub mod pricing;
pub mod protocol_types;
pub mod quote_signer;
pub mod secrets;
pub mod sui_client;
pub mod tx;
pub mod ws_client;

// -- Convenience re-exports ----------------------------------------------

pub use deployments::{Deployments, NetworkDeployment, TestTokens, TokenInfo};
pub use protocol_types::{
    AssetType, ObjectId, ProtocolError, Quote, SignedQuote, SigningScheme, Side, SuiAddress,
};
pub use protocol_types::signing_scheme::verify_signature;
pub use quote_signer::QuoteSigner;
pub use secrets::Secrets;
pub use sui_client::{Network, Signer, SuiClientWrapper};
