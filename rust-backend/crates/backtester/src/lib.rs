//! Thin-slice conditional backtester for the long-option desk
//! (docs/mm-bot-v2/09-backtesting-gap-remediation.md, G4).
//!
//! Scope of v0, deliberately small: gold 1-minute bars are the market
//! path, a configured oracle model degrades them into the decision price
//! (`proxy_oracle`), fills are taker-only at a configured spread, one
//! signed net-delta book of bought calls and puts is band-hedged with a
//! perp, funding accrues against the signed position at every settlement
//! row, options settle at expiry (`exercise=at_expiry`), and a simple
//! ledger reconciles cash + option marks + perp P&L to NAV every minute.
//! Flow is a constant injector (doc 08 §8 capacity-mode subset); the full
//! arrival/acceptance generator is doc 08 PR N. Every output carries its
//! labels and a determinism hash.
//!
//! Nothing here consumes oracle-provider history (doc 09 §3): the decision
//! price is lake mids through the configured model, and it says so.

pub mod data;
pub mod engine;
pub mod estimator;
pub mod flow;
pub mod ledger;
pub mod model;
pub mod oracle;
pub mod report;
pub mod scenario;

pub const MS_PER_DAY: i64 = 86_400_000;
pub const MS_PER_YEAR_F: f64 = 365.0 * 86_400_000.0;

/// FNV-1a over bytes — the determinism hash printed with every run (doc
/// 08 §1 item 7: same data, config, and seed ⇒ byte-identical output).
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}
