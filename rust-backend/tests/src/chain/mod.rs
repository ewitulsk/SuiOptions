//! Full-stack chain harness — built out in Phase 2.
//!
//! Composition (target):
//!
//! ```text
//!  LocalNet ── publishes ──► Deployments ──► IndexerService ──► QuotingService
//!     │                                                              ▲
//!     └── faucet ──► gas wallets ──► RealMmBot / MockMm / Writer ────┘
//! ```
//!
//! Each test gets a fresh [`env::TestEnv`] via `TestEnv::fresh().await`.
//! Submodules below are stubbed out in Phase 0 and filled in Phase 2b.

pub mod actors;
pub mod deploy;
pub mod env;
pub mod localnet;
pub mod postgres;
pub mod services;
