//! Domain types for the hybrid exchange orderbook.
//!
//! Everything here is chain-adjacent: `Order` BCS-encodes byte-identically to
//! the Move `exchange::order::Order` struct (field order is consensus-critical),
//! amounts are `u64` and all price arithmetic goes through `u128` intermediates
//! with explicit rounding direction — no floating point anywhere.

pub mod market;
pub mod math;
pub mod order;
pub mod types;

pub use market::{Market, MarketId, Side};
pub use order::{Order, SignedOrder};
pub use types::{canonicalize_move_type, Digest, ObjectId, SuiAddress, TypeTagStr};
