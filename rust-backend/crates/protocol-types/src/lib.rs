//! Shared types for the off-chain Sui covered-call options stack.
//!
//! Two encodings matter here:
//!
//! - **BCS** — the canonical bytes the on-chain Move runtime hashes and
//!   verifies. Anything that crosses the chain boundary (notably [`quote::Quote`])
//!   must round-trip through BCS byte-for-byte against §3.2.7 of the protocol
//!   spec.
//! - **JSON** — the human-readable wire format used over WebSocket. Numbers
//!   are encoded as decimal strings to dodge JS precision loss (§4.3) and
//!   32-byte identifiers as `0x`-prefixed hex.
//!
//! [`ObjectId`], [`SuiAddress`], and the integer helpers in [`coding`] switch
//! between the two via `serde`'s `is_human_readable` flag.

pub mod asset;
pub mod coding;
pub mod errors;
pub mod events;
pub mod ids;
pub mod messages;
pub mod pyth_id;
pub mod quote;
pub mod sides;
pub mod signing_scheme;

pub use asset::AssetType;
pub use errors::ProtocolError;
pub use ids::{ObjectId, SuiAddress};
pub use pyth_id::PriceFeedId;
pub use quote::{Quote, SignedQuote};
pub use sides::Side;
pub use signing_scheme::{verify_signature, SigningScheme};
