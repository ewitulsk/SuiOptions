//! Realized-vol source for the z-ladder grid (doc 05 §4.2). The
//! implementation moved to `pyth_client::sigma` so the vault-keeper can
//! share it without depending on this crate; re-exported here to keep
//! the scheduler's call sites unchanged.

pub use pyth_client::sigma::realized_sigma_from_benchmarks;
