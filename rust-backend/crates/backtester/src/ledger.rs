//! The backtester's ledger IS the desk's exact ledger
//! (`desk_core::ledger`, doc 08 §5.3 / PR J): cash, spot inventory, open
//! options at their marks, the signed perp (average-entry accounting,
//! isolated margin), live reservations, pending PTBs / transfers, and
//! the accounting lines that reconcile to NAV after every event. What
//! lives here is only what the SIMULATION needs on top of it: the
//! per-option study data (`Study`), the engine-side decline counters,
//! and the option-id convention.

use std::collections::BTreeMap;

pub use desk_core::ledger::{
    ExercisePath, ExercisePlan, Ledger, LedgerEvent, Lines, OptionKind, OptionPosition, OptionSpec,
    PerpPosition, Violation,
};
use serde::Serialize;

/// Ledger option id of simulated position `id` (zero-padded so the
/// ledger's sorted maps iterate in creation order, as v0's `Vec` did).
pub fn option_id(id: u64) -> String {
    format!("{id:012}")
}

/// Per-option study data for the vol-P&L rows (doc 09 §2.4) — what the
/// ledger does not carry: the sigma the bid was struck at, entry greeks
/// and the exits-cadence bookkeeping.
#[derive(Clone, Debug, Serialize)]
pub struct Study {
    pub id: u64,
    pub is_put: bool,
    pub strike: f64,
    pub expiry_ms: i64,
    pub premium_paid: f64,
    /// Sigma the bid was struck at (after the vol discount).
    pub sigma_paid: f64,
    /// Surface (pre-discount) sigma at entry.
    pub sigma_surface: f64,
    pub opened_ms: i64,
    pub spot_open: f64,
    pub delta_open: f64,
    pub gamma_open: f64,
    pub vega_open: f64,
    /// Writer-side net premium after the protocol fee (the displayed APY
    /// base) — doc 09 G7.
    pub writer_net_premium: f64,
    /// Units at entry (exercise slices reduce the ledger line).
    pub qty_open: f64,
    /// Last exits-cadence check (the daily check; the near-expiry sweep
    /// ignores it).
    pub last_check_ms: i64,
    /// Settlement realized by exercise slices so far.
    pub exercise_net: f64,
}

/// Decisions the strategy made that move no balance (v0's `Lines`
/// counters that are not accounting).
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Counters {
    pub declines_capacity: u64,
    pub declines_stale: u64,
    pub declines_priced_zero: u64,
    /// Top-ups the desk declined: 24 h cap or no free cash.
    pub topup_declines: u64,
    /// Hedge orders not sent because the entry margin could not be
    /// funded from free cash (no risk without margin).
    pub hedge_declines_margin: u64,
}

/// Marked premium in options of one type.
pub fn premium_by_type(l: &Ledger, is_put: bool) -> f64 {
    l.options.values().filter(|p| p.spec.kind.is_put() == is_put).map(OptionPosition::value).sum()
}

/// Marked premium per expiry.
pub fn premium_by_expiry(l: &Ledger) -> BTreeMap<i64, f64> {
    let mut out = BTreeMap::new();
    for p in l.options.values() {
        *out.entry(p.spec.expiry_ms as i64).or_default() += p.value();
    }
    out
}
