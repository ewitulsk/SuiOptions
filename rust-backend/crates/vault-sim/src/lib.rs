//! Backtesting engine for the covered-call vault
//! (docs/vault-implementation-guide/06-vault-sim.md).
//!
//! Two layers, deliberately separated:
//!
//! - **Accounting runs in integers.** `cursor` mirrors `bucket.move`'s
//!   write/exercise/redeem mechanics and `ledger` mirrors the vault's round
//!   accounting (doc 03 §7.4) in chain smallest-units with identical
//!   rounding — this crate doubles as the reference implementation the Move
//!   contracts are diffed against (golden-file cross-validation, doc 06
//!   §9.3).
//! - **Pricing runs in f64.** IV proxies, premium models, and sale
//!   mechanisms are swept scenario parameters, not point estimates.

pub mod cursor;
pub mod engine;
pub mod exercise;
pub mod iv;
pub mod ledger;
pub mod metrics;
pub mod paths;
pub mod premium;
pub mod sale;
pub mod strategy;
pub mod swap;
pub mod types;
