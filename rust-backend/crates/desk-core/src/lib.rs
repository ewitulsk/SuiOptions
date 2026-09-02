//! `desk-core` — the mm-bot vol desk's pure strategy kernel (doc 08 §2 /
//! §5.2, SO-450): every piece of desk state and policy that decides
//! quotes, reservations, hedges and exits, with no I/O of its own.
//!
//! ```text
//!                     ┌────────────────────┐
//!  external event ───▶│ DeskKernel::on_event│──▶ commands
//!                     └────────────────────┘
//!         live adapters (services/mm-bot)   simulation adapters (desk-backtester)
//!         WS / Sui / Bluefin                clock / fills / failures
//! ```
//!
//! What lives here (doc 08 §5.2's "moves to desk-core" column):
//!
//! - [`kernel`] — [`DeskKernel`], the [`Event`] set (§2.1) and the
//!   [`Command`] set (§2.2).
//! - [`quote`] — RFQ inputs and the writer-flow decision.
//! - [`limits`] — continuous-utilization limits, composition throttles,
//!   the kill-switch state, [`limits::CapitalSnapshot`] /
//!   [`limits::CapitalPolicy`] (SO-444/445).
//! - [`model`] — per-market pricing model over the vol surface and the
//!   rolling-vol / HAR estimators (`RollingVolBuffer` is
//!   `vol_forecast`'s, the ONE shared implementation).
//! - [`book`] — holdings, written lines, the durable-reservation state
//!   machine (the DB writer stays in mm-bot), P&L attribution counters.
//! - [`exposure`] — the mark pass: holdings → marks, greeks, exposure and
//!   the capital snapshot inputs.
//! - [`exits`] / [`exits::put`] — call and put exit policy.
//! - [`hedge`] — signed hedge policy: band math, `plan_hedge_order`,
//!   `OpenOrders`.
//!
//! Deterministic and I/O-free by construction: no tokio, reqwest, diesel
//! or Sui SDK dependency (`tests::dependency_assertion`), no clocks (every
//! event carries its timestamp), no maps iterated in observable order
//! without sorting.

pub mod book;
pub mod exits;
pub mod exposure;
pub mod hedge;
pub mod kernel;
pub mod limits;
pub mod model;
pub mod quote;

pub use kernel::{Command, DeskKernel, Event, KernelConfig};
pub use vol_forecast::RollingVolBuffer;

#[cfg(test)]
mod tests {
    /// The kernel may never grow a network, database, async-runtime or
    /// chain-SDK dependency: the same code must run byte-identically
    /// under the live adapters and the backtester.
    #[test]
    fn dependency_assertion() {
        let manifest = include_str!("../Cargo.toml");
        let deps = manifest
            .split("[dependencies]")
            .nth(1)
            .expect("[dependencies] section")
            .split("[dev-dependencies]")
            .next()
            .unwrap();
        for banned in ["tokio", "reqwest", "diesel", "sui-", "sui_", "async-trait", "hyper", "axum"] {
            assert!(
                !deps.lines().any(|l| l.trim_start().starts_with(banned)),
                "desk-core must not depend on {banned}:\n{deps}"
            );
        }
    }
}
