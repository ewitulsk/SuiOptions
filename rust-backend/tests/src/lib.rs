//! Cross-crate integration tests for the options backend.
//!
//! Two layers:
//!
//! - [`router`] — in-process harness: real quoting service + a fake indexer
//!   fanout backed by [`indexer::Store::ingest`]. Fast, hermetic, runs on
//!   every `cargo test`. Good coverage of WS plumbing, RFQ orchestration,
//!   reservation/balance bookkeeping. Bypasses the chain — events are
//!   handed to the store as already-decoded `ChainEvent`s.
//!
//! - [`chain`] (feature `chain`) — full-stack e2e against a per-test
//!   `sui-test-validator` localnet plus an ephemeral Postgres. Exercises
//!   PTBs, BCS, the real indexer worker, the fanout, and the real
//!   quoting-service in one process. Slow (~30s per test) and needs `sui`
//!   + Docker locally. Off by default; opt in with `--features chain`.

pub mod router;

#[cfg(feature = "chain")]
pub mod chain;

// Re-export router helpers at the crate root for backward compatibility
// with the existing tests that did `use integration_tests::*`.
pub use router::*;
