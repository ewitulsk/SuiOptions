//! Economic attribution (doc 08 §2.3, §9.1): explains NAV changes, never
//! defines NAV. The exact ledger is the truth; every line here is either
//! a cash flow the ledger already booked or a mark decomposition that
//! reconciles to the ledger by construction:
//!
//! ```text
//! option leg = spread (ledger)      = model edge at entry, per type here
//!            + option_mark (ledger) = mark-to-market while held, expiry to zero
//!                                     (Δ, Γ, Θ, V explained + residual)
//!            + option_exit (ledger) = settlement realized − mark given up
//! perp leg (realized + unrealized)  = delta + basis − slippage + residual
//! ΔNAV = Δ lines.realized() + Δ option_mark + Δ perp unrealized + Δ equity flows
//!        (the ledger's own `nav_explained` identity, doc 08 §5.3)
//! ```
//!
//! The totals are the shared ledger's (`desk_core::ledger::Lines`); the
//! accumulator only splits them by option type and Greek, and the report
//! asserts the split sums back to the ledger.
//!
//! Model edge is explicitly NOT realized revenue (§12 item 15): it is the
//! difference between the desk's own fair value and what it paid, and it
//! only becomes money if the fair value was right.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::model::Greeks;
use crate::{MS_PER_DAY, MS_PER_YEAR_F};

/// Cumulative attribution lines, settlement units. Costs are positive
/// where the ledger books them as costs; P&L lines are signed.
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq)]
pub struct AttrLines {
    /// Σ (fair at fill − bid paid), by type. Non-realized.
    pub model_edge_call: f64,
    pub model_edge_put: f64,
    /// Σ mark changes while held (marks at the revalue cadence), by type.
    pub option_mtm_call: f64,
    pub option_mtm_put: f64,
    /// Σ (payoff − last mark × units) at exercise/expiry/resale, by type.
    pub exit_vs_mark_call: f64,
    pub exit_vs_mark_put: f64,
    /// Greek explanation of the option marks (explanation cadence).
    pub option_delta: f64,
    pub option_gamma: f64,
    pub option_theta: f64,
    pub option_vega: f64,
    /// `option_mtm − Σ explained`: higher-order + between-explanation stretches.
    pub option_residual: f64,
    /// Perp explanation: position × Δspot, position × Δ(mark − spot).
    pub perp_delta: f64,
    pub perp_basis: f64,
    /// Perp P&L not explained by delta, basis and booked slippage.
    pub perp_residual: f64,
    /// Perp realized + unrealized change (ledger truth, cumulative).
    pub perp_pnl: f64,
    /// Funding split by the signed hedge direction (paid > 0).
    pub funding_paid_long: f64,
    pub funding_paid_short: f64,
    /// Exercise: intrinsic at the decision price minus the net received.
    pub exercise_cost: f64,
    pub exercise_intrinsic: f64,
    /// Idle settlement cash × cash yield × dt (opportunity cost; NOT in NAV).
    pub idle_cash_cost: f64,
    /// Explanation intervals and the largest single-interval residual.
    pub explain_steps: u64,
    pub max_abs_step_residual: f64,
}

impl AttrLines {
    pub fn model_edge(&self) -> f64 {
        self.model_edge_call + self.model_edge_put
    }

    pub fn option_mtm(&self) -> f64 {
        self.option_mtm_call + self.option_mtm_put
    }

    pub fn exit_vs_mark(&self) -> f64 {
        self.exit_vs_mark_call + self.exit_vs_mark_put
    }

    pub fn sub(&self, o: &AttrLines) -> AttrLines {
        AttrLines {
            model_edge_call: self.model_edge_call - o.model_edge_call,
            model_edge_put: self.model_edge_put - o.model_edge_put,
            option_mtm_call: self.option_mtm_call - o.option_mtm_call,
            option_mtm_put: self.option_mtm_put - o.option_mtm_put,
            exit_vs_mark_call: self.exit_vs_mark_call - o.exit_vs_mark_call,
            exit_vs_mark_put: self.exit_vs_mark_put - o.exit_vs_mark_put,
            option_delta: self.option_delta - o.option_delta,
            option_gamma: self.option_gamma - o.option_gamma,
            option_theta: self.option_theta - o.option_theta,
            option_vega: self.option_vega - o.option_vega,
            option_residual: self.option_residual - o.option_residual,
            perp_delta: self.perp_delta - o.perp_delta,
            perp_basis: self.perp_basis - o.perp_basis,
            perp_residual: self.perp_residual - o.perp_residual,
            perp_pnl: self.perp_pnl - o.perp_pnl,
            funding_paid_long: self.funding_paid_long - o.funding_paid_long,
            funding_paid_short: self.funding_paid_short - o.funding_paid_short,
            exercise_cost: self.exercise_cost - o.exercise_cost,
            exercise_intrinsic: self.exercise_intrinsic - o.exercise_intrinsic,
            idle_cash_cost: self.idle_cash_cost - o.idle_cash_cost,
            explain_steps: self.explain_steps - o.explain_steps,
            max_abs_step_residual: self.max_abs_step_residual.max(o.max_abs_step_residual),
        }
    }
}

/// Per-position state at the last explanation instant.
#[derive(Clone, Copy, Debug)]
struct PrevState {
    spot: f64,
    sigma: f64,
    mark: f64,
    ms: i64,
    g: Greeks,
}

/// The engine-side accumulator: hooks called from the event loop.
#[derive(Clone, Debug)]
pub struct Accum {
    pub lines: AttrLines,
    prev: BTreeMap<u64, PrevState>,
    /// Perp state at the last explanation instant: (position, spot, mark,
    /// realized + unrealized, booked slippage).
    perp_prev: Option<(f64, f64, f64, f64, f64)>,
    last_explain_ms: i64,
    explain_ms: i64,
    cash_yield: f64,
    last_idle_ms: i64,
}

impl Accum {
    pub fn new(explain_interval_min: i64, cash_yield: f64) -> Self {
        Self {
            lines: AttrLines::default(),
            prev: BTreeMap::new(),
            perp_prev: None,
            last_explain_ms: i64::MIN,
            explain_ms: explain_interval_min.max(1) * 60_000,
            cash_yield,
            last_idle_ms: i64::MIN,
        }
    }

    /// A fill: the model edge at entry (fair at the surface sigma − bid).
    pub fn on_fill(&mut self, is_put: bool, fair: f64, bid: f64) {
        let edge = fair - bid;
        if is_put { self.lines.model_edge_put += edge } else { self.lines.model_edge_call += edge }
    }

    /// A revalue moved one position's mark.
    pub fn on_mark_change(&mut self, is_put: bool, d_mark_times_qty: f64) {
        if is_put { self.lines.option_mtm_put += d_mark_times_qty } else { self.lines.option_mtm_call += d_mark_times_qty }
    }

    /// Units leave the book for `net` settlement against `last_mark`.
    pub fn on_exit(&mut self, id: u64, is_put: bool, units: f64, net: f64, last_mark: f64, remaining_units: f64) {
        let d = net - last_mark * units;
        if is_put { self.lines.exit_vs_mark_put += d } else { self.lines.exit_vs_mark_call += d }
        if remaining_units <= 1e-12 {
            self.prev.remove(&id);
        }
    }

    pub fn on_exercise(&mut self, intrinsic: f64, net: f64) {
        self.lines.exercise_intrinsic += intrinsic;
        self.lines.exercise_cost += intrinsic - net;
    }

    pub fn on_funding(&mut self, position: f64, paid: f64) {
        if position > 0.0 { self.lines.funding_paid_long += paid } else if position < 0.0 { self.lines.funding_paid_short += paid }
    }

    /// Idle-cash opportunity cost at the settlement cash yield, accrued
    /// on positive free cash between samples.
    pub fn on_cash_sample(&mut self, now_ms: i64, cash: f64) {
        if self.last_idle_ms != i64::MIN && now_ms > self.last_idle_ms {
            let dt = (now_ms - self.last_idle_ms) as f64 / MS_PER_YEAR_F;
            self.lines.idle_cash_cost += cash.max(0.0) * self.cash_yield * dt;
        }
        self.last_idle_ms = now_ms;
    }

    pub fn explain_due(&self, now_ms: i64) -> bool {
        now_ms.saturating_sub(self.last_explain_ms) >= self.explain_ms
    }

    /// One explanation step. `book`: every open position as
    /// `(id, is_put, qty, mark, spot, sigma, greeks)` at `now`; `perp`:
    /// `(position, spot, mark, realized + unrealized, booked slippage)`.
    pub fn explain(&mut self, now_ms: i64, book: &[(u64, bool, f64, f64, f64, f64, Greeks)], perp: (f64, f64, f64, f64, f64)) {
        self.last_explain_ms = now_ms;
        let mut step_explained = 0.0;
        let mut step_actual = 0.0;
        let mut seen = std::collections::BTreeSet::new();
        for &(id, _is_put, qty, mark, spot, sigma, g) in book {
            seen.insert(id);
            if let Some(p) = self.prev.get(&id) {
                let ds = spot - p.spot;
                let dt_days = (now_ms - p.ms) as f64 / MS_PER_DAY as f64;
                let dsig = sigma - p.sigma;
                let delta = p.g.delta * ds * qty;
                let gamma = 0.5 * p.g.gamma * ds * ds * qty;
                let theta = p.g.theta * dt_days * qty;
                let vega = p.g.vega * dsig * qty;
                self.lines.option_delta += delta;
                self.lines.option_gamma += gamma;
                self.lines.option_theta += theta;
                self.lines.option_vega += vega;
                step_explained += delta + gamma + theta + vega;
                step_actual += (mark - p.mark) * qty;
            }
            self.prev.insert(id, PrevState { spot, sigma, mark, ms: now_ms, g });
        }
        self.prev.retain(|id, _| seen.contains(id));
        let (pos, spot, mark, pnl, slip) = perp;
        match self.perp_prev {
            Some((pp, ps, pm, ppnl, pslip)) => {
                let d = pp * (spot - ps);
                let b = pp * ((mark - spot) - (pm - ps));
                let actual = pnl - ppnl;
                let slipped = slip - pslip;
                self.lines.perp_delta += d;
                self.lines.perp_basis += b;
                self.lines.perp_residual += actual - d - b + slipped;
            }
            // Anything before the first step is unexplained by definition.
            None => self.lines.perp_residual += pnl + slip,
        }
        self.perp_prev = Some((pos, spot, mark, pnl, slip));
        self.lines.perp_pnl = pnl;
        self.lines.explain_steps += 1;
        let r = step_actual - step_explained;
        self.lines.max_abs_step_residual = self.lines.max_abs_step_residual.max(r.abs());
    }

    /// Close the books: the residual is whatever mark-to-market the
    /// explanation steps did not cover.
    pub fn finish(&mut self, perp_pnl: f64, slippage: f64) {
        // The stretch after the last explanation step.
        match self.perp_prev {
            Some((_, _, _, ppnl, pslip)) => self.lines.perp_residual += (perp_pnl - ppnl) + (slippage - pslip),
            None => self.lines.perp_residual += perp_pnl + slippage,
        }
        self.lines.perp_pnl = perp_pnl;
        let explained = self.lines.option_delta + self.lines.option_gamma + self.lines.option_theta + self.lines.option_vega;
        self.lines.option_residual = self.lines.option_mtm() - explained;
    }
}

/// A daily snapshot the per-regime / per-turn views are differenced from.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct DailyAttr {
    pub ts_ms: i64,
    pub spot: f64,
    pub nav: f64,
    pub cash: f64,
    pub option_marks: f64,
    pub perp_unrealized: f64,
    /// `EquityFlows::total()` (zero in simulation: no re-syncs).
    pub equity_flows: f64,
    pub lines: crate::ledger::Lines,
    pub attr: AttrLines,
    pub fills: u64,
    pub fills_call: u64,
    pub fills_put: u64,
}

/// One attribution window (cumulative, a turn, a regime, an option type).
#[derive(Clone, Debug, Serialize)]
pub struct Window {
    pub label: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub days: f64,
    pub nav_start: f64,
    pub nav_end: f64,
    pub spot_start: f64,
    pub spot_end: f64,
    /// Exact realized change of NAV (ledger truth).
    pub nav_change: f64,
    pub return_exact: f64,
    pub return_annualized: f64,
    /// Return after the idle-cash opportunity cost (doc 08 §9.1).
    pub return_exact_after_idle_cost: f64,
    pub return_annualized_after_idle_cost: f64,
    pub fills: u64,
    pub fills_call: u64,
    pub fills_put: u64,
    // ── option leg ──
    pub premium_paid: f64,
    pub option_payoff: f64,
    pub option_leg_pnl: f64,
    pub model_edge_at_entry_non_realized: f64,
    pub option_mtm: f64,
    pub exit_vs_mark: f64,
    // ── perp leg ──
    pub perp_realized: f64,
    pub perp_unrealized_change: f64,
    pub funding_paid_long: f64,
    pub funding_paid_short: f64,
    pub funding_paid: f64,
    // ── costs ──
    pub taker_fees: f64,
    pub maker_fees: f64,
    pub slippage: f64,
    pub gas: f64,
    pub fixed_costs: f64,
    pub exercise_cost: f64,
    pub liquidation_loss: f64,
    pub idle_cash_cost: f64,
    // ── explanation ──
    pub delta: f64,
    pub gamma: f64,
    pub theta: f64,
    pub vega: f64,
    pub basis: f64,
    pub residual: f64,
    pub residual_pct_of_gross: f64,
    /// Σ lines − nav_change: zero when the ledger reconciles.
    pub reconciliation_gap: f64,
}

/// Window over `[a, b]` daily snapshots (b inclusive) — `a` is the state at
/// the start (cumulative lines before the window), `b` at the end.
pub fn window(label: &str, a: &DailyAttr, b: &DailyAttr, fixed_fee_per_fill: f64) -> Window {
    let l = b.lines;
    let l0 = a.lines;
    let at = b.attr.sub(&a.attr);
    let days = (b.ts_ms - a.ts_ms) as f64 / MS_PER_DAY as f64;
    let years = days / 365.0;
    let nav_change = b.nav - a.nav;
    let ret = if a.nav > 0.0 { nav_change / a.nav } else { 0.0 };
    let ann = |r: f64| if years > 0.0 && r > -1.0 { (1.0 + r).powf(1.0 / years) - 1.0 } else if r <= -1.0 { -1.0 } else { 0.0 };
    let premium = l.premium_paid - l0.premium_paid;
    let payoff = l.option_payoff - l0.option_payoff;
    let perp_realized = l.hedge_realized - l0.hedge_realized;
    let perp_unreal = b.perp_unrealized - a.perp_unrealized;
    let funding = l.funding_paid - l0.funding_paid;
    let taker = l.hedge_fees - l0.hedge_fees;
    let maker = l.maker_fees - l0.maker_fees;
    let slip = l.hedge_slippage - l0.hedge_slippage;
    let gas = l.gas - l0.gas;
    let liq = l.liquidation_loss - l0.liquidation_loss;
    let fills = (l.hedge_fills - l0.hedge_fills) as f64;
    let fixed = fills * fixed_fee_per_fill;
    // The ledger's identity: ΔNAV = Δrealized lines + Δoption marks +
    // Δperp unrealized + Δequity flows (`Ledger::nav_explained`).
    let sum = (l.realized() - l0.realized()) + (l.option_mark - l0.option_mark) + perp_unreal + (b.equity_flows - a.equity_flows);
    let gross = premium.abs() + payoff.abs() + perp_realized.abs() + perp_unreal.abs();
    let residual = at.option_residual + at.perp_residual;
    let nav_after_idle = b.nav - at.idle_cash_cost;
    let ret_idle = if a.nav > 0.0 { nav_after_idle / a.nav - 1.0 } else { 0.0 };
    Window {
        label: label.to_string(),
        from_ms: a.ts_ms,
        to_ms: b.ts_ms,
        days,
        nav_start: a.nav,
        nav_end: b.nav,
        spot_start: a.spot,
        spot_end: b.spot,
        nav_change,
        return_exact: ret,
        return_annualized: ann(ret),
        return_exact_after_idle_cost: ret_idle,
        return_annualized_after_idle_cost: ann(ret_idle),
        fills: b.fills - a.fills,
        fills_call: b.fills_call - a.fills_call,
        fills_put: b.fills_put - a.fills_put,
        premium_paid: premium,
        option_payoff: payoff,
        option_leg_pnl: payoff - premium,
        model_edge_at_entry_non_realized: at.model_edge(),
        option_mtm: at.option_mtm(),
        exit_vs_mark: at.exit_vs_mark(),
        perp_realized,
        perp_unrealized_change: perp_unreal,
        funding_paid_long: at.funding_paid_long,
        funding_paid_short: at.funding_paid_short,
        funding_paid: funding,
        taker_fees: taker,
        maker_fees: maker,
        slippage: slip,
        gas,
        fixed_costs: fixed,
        exercise_cost: at.exercise_cost,
        liquidation_loss: liq,
        idle_cash_cost: at.idle_cash_cost,
        delta: at.option_delta + at.perp_delta,
        gamma: at.option_gamma,
        theta: at.option_theta,
        vega: at.option_vega,
        basis: at.perp_basis,
        residual,
        residual_pct_of_gross: if gross > 0.0 { residual.abs() / gross } else { 0.0 },
        reconciliation_gap: sum - nav_change,
    }
}

/// Option-type split of the lines that are separable per type (the hedge
/// is one netted book and is reported once, at the book level).
#[derive(Clone, Debug, Serialize)]
pub struct TypeView {
    pub option_type: &'static str,
    pub fills: u64,
    pub settled: usize,
    pub premium_paid: f64,
    pub option_payoff: f64,
    pub option_leg_pnl: f64,
    pub model_edge_at_entry_non_realized: f64,
    pub option_mtm: f64,
    pub exit_vs_mark: f64,
    pub mean_sigma_paid: f64,
    pub mean_sigma_realized: f64,
    pub exercise_paths: BTreeMap<String, u64>,
    pub note: &'static str,
}

/// Market regime of a day: trailing-30-day return bucket × vol tier.
pub fn regime_labels(daily: &[DailyAttr]) -> Vec<String> {
    let n = daily.len();
    let mut rv = vec![0.0; n];
    let mut ret = vec![0.0; n];
    for i in 0..n {
        let j = i.saturating_sub(30);
        ret[i] = if daily[j].spot > 0.0 { daily[i].spot / daily[j].spot - 1.0 } else { 0.0 };
        let lo = i.saturating_sub(30).max(1);
        let mut acc = 0.0;
        let mut k = 0;
        for t in lo..=i {
            if t >= 1 && daily[t - 1].spot > 0.0 && daily[t].spot > 0.0 {
                let r = (daily[t].spot / daily[t - 1].spot).ln();
                acc += r * r;
                k += 1;
            }
        }
        rv[i] = if k > 0 { (acc / k as f64 * 365.0).sqrt() } else { 0.0 };
    }
    let mut sorted: Vec<f64> = rv.iter().copied().filter(|v| *v > 0.0).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = if sorted.is_empty() { 0.0 } else { sorted[sorted.len() / 2] };
    (0..n)
        .map(|i| {
            let dir = if ret[i] <= -0.20 { "crash" } else if ret[i] >= 0.20 { "rally" } else { "range" };
            let vol = if rv[i] > med { "high_vol" } else { "low_vol" };
            format!("{dir}/{vol}")
        })
        .collect()
}

/// Group consecutive days by regime label and difference the snapshots.
pub fn by_regime(daily: &[DailyAttr], fixed_fee: f64) -> Vec<Window> {
    let labels = regime_labels(daily);
    let mut out: Vec<Window> = Vec::new();
    let mut i = 1;
    while i < daily.len() {
        let l = &labels[i];
        let start = i - 1;
        let mut end = i;
        while end + 1 < daily.len() && labels[end + 1] == *l {
            end += 1;
        }
        out.push(window(l, &daily[start], &daily[end], fixed_fee));
        i = end + 1;
    }
    // Merge same-label spans into one line per regime (sums are linear).
    let mut merged: BTreeMap<String, Window> = BTreeMap::new();
    for w in out {
        match merged.get_mut(&w.label) {
            None => {
                merged.insert(w.label.clone(), w);
            }
            Some(m) => merge_into(m, &w),
        }
    }
    merged.into_values().collect()
}

fn merge_into(m: &mut Window, w: &Window) {
    let a = m.nav_start;
    m.days += w.days;
    m.to_ms = m.to_ms.max(w.to_ms);
    m.nav_change += w.nav_change;
    m.nav_end = a + m.nav_change;
    // Chain-linked returns over the disjoint spans.
    let years = m.days / 365.0;
    let ann = |r: f64| if years > 0.0 && r > -1.0 { (1.0 + r).powf(1.0 / years) - 1.0 } else { -1.0 };
    m.return_exact = (1.0 + m.return_exact) * (1.0 + w.return_exact) - 1.0;
    m.return_annualized = ann(m.return_exact);
    m.return_exact_after_idle_cost = (1.0 + m.return_exact_after_idle_cost) * (1.0 + w.return_exact_after_idle_cost) - 1.0;
    m.return_annualized_after_idle_cost = ann(m.return_exact_after_idle_cost);
    for (x, y) in [
        (&mut m.premium_paid, w.premium_paid), (&mut m.option_payoff, w.option_payoff), (&mut m.option_leg_pnl, w.option_leg_pnl),
        (&mut m.model_edge_at_entry_non_realized, w.model_edge_at_entry_non_realized), (&mut m.option_mtm, w.option_mtm),
        (&mut m.exit_vs_mark, w.exit_vs_mark), (&mut m.perp_realized, w.perp_realized), (&mut m.perp_unrealized_change, w.perp_unrealized_change),
        (&mut m.funding_paid_long, w.funding_paid_long), (&mut m.funding_paid_short, w.funding_paid_short), (&mut m.funding_paid, w.funding_paid),
        (&mut m.taker_fees, w.taker_fees), (&mut m.maker_fees, w.maker_fees), (&mut m.slippage, w.slippage), (&mut m.gas, w.gas),
        (&mut m.fixed_costs, w.fixed_costs), (&mut m.exercise_cost, w.exercise_cost), (&mut m.liquidation_loss, w.liquidation_loss),
        (&mut m.idle_cash_cost, w.idle_cash_cost), (&mut m.delta, w.delta), (&mut m.gamma, w.gamma), (&mut m.theta, w.theta),
        (&mut m.vega, w.vega), (&mut m.basis, w.basis), (&mut m.residual, w.residual), (&mut m.reconciliation_gap, w.reconciliation_gap),
    ] {
        *x += y;
    }
    m.fills += w.fills;
    m.fills_call += w.fills_call;
    m.fills_put += w.fills_put;
    let gross = m.premium_paid.abs() + m.option_payoff.abs() + m.perp_realized.abs() + m.perp_unrealized_change.abs();
    m.residual_pct_of_gross = if gross > 0.0 { m.residual.abs() / gross } else { 0.0 };
}

/// Windows between consecutive boundaries (turn starts or month starts).
pub fn by_boundaries(daily: &[DailyAttr], fixed_fee: f64, boundaries_ms: &[i64], prefix: &str) -> Vec<Window> {
    let mut out = Vec::new();
    if daily.len() < 2 {
        return out;
    }
    // A boundary lands on the last snapshot at or before it, so the fill
    // that opens a turn is inside that turn.
    let idx = |ms: i64| daily.iter().rposition(|d| d.ts_ms <= ms).unwrap_or(0);
    let mut cuts: Vec<usize> = boundaries_ms.iter().map(|&b| idx(b)).collect();
    cuts.push(daily.len() - 1);
    cuts.sort_unstable();
    cuts.dedup();
    if cuts.first().copied() != Some(0) {
        cuts.insert(0, 0);
    }
    for (k, pair) in cuts.windows(2).enumerate() {
        let (a, b) = (pair[0], pair[1]);
        if b > a {
            out.push(window(&format!("{prefix}{}", k + 1), &daily[a], &daily[b], fixed_fee));
        }
    }
    out
}

/// Month starts (UTC) covering the daily path.
pub fn month_boundaries(daily: &[DailyAttr]) -> Vec<i64> {
    let mut out = Vec::new();
    let mut last = (0i32, 0u32);
    for d in daily {
        let dt = chrono::DateTime::from_timestamp_millis(d.ts_ms).expect("ts");
        let ym = (chrono::Datelike::year(&dt), chrono::Datelike::month(&dt));
        if ym != last {
            last = ym;
            out.push(d.ts_ms);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explanation_reconciles_a_synthetic_book_and_bounds_the_residual() {
        // One call, marked by the model at two instants: the explanation
        // must land within a small residual of the actual mark change.
        let (spot0, k, t0, sig0) = (100.0, 100.0, 30.0 / 365.0, 0.8);
        let g0 = crate::model::greeks_per_unit(false, spot0, k, t0, sig0, 0.0);
        let m0 = crate::model::fair_per_unit(false, spot0, k, t0, sig0, 0.0);
        let mut a = Accum::new(60, 0.04);
        a.explain(0, &[(1, false, 10.0, m0, spot0, sig0, g0)], (-5.0, spot0, spot0, 0.0, 0.0));
        let (spot1, t1, sig1) = (101.0, t0 - 1.0 / 365.0, 0.82);
        let g1 = crate::model::greeks_per_unit(false, spot1, k, t1, sig1, 0.0);
        let m1 = crate::model::fair_per_unit(false, spot1, k, t1, sig1, 0.0);
        a.on_mark_change(false, (m1 - m0) * 10.0);
        let mark1 = spot1 * 1.001;
        a.explain(MS_PER_DAY, &[(1, false, 10.0, m1, spot1, sig1, g1)], (-5.0, spot1, mark1, -5.0 * (mark1 - spot0), 0.0));
        a.finish(-5.0 * (mark1 - spot0), 0.0);
        let l = a.lines;
        let explained = l.option_delta + l.option_gamma + l.option_theta + l.option_vega;
        assert!((l.option_mtm() - explained - l.option_residual).abs() < 1e-9);
        assert!(l.option_residual.abs() < 0.05 * l.option_mtm().abs().max(1.0), "residual {} vs mtm {}", l.option_residual, l.option_mtm());
        assert!(l.option_delta > 0.0 && l.option_theta < 0.0 && l.option_vega > 0.0 && l.option_gamma > 0.0);
        // Perp: short 5 from spot 100 → 101 = −5 delta; basis 0.1% × 101 × −5.
        assert!((l.perp_delta - (-5.0)).abs() < 1e-9);
        assert!((l.perp_basis - (-5.0 * 0.101)).abs() < 1e-9);
        assert!(l.perp_residual.abs() < 1e-9, "{}", l.perp_residual);
        assert_eq!(l.explain_steps, 2);
    }

    #[test]
    fn idle_cash_accrues_at_the_cash_yield() {
        let mut a = Accum::new(60, 0.05);
        a.on_cash_sample(0, 1000.0);
        a.on_cash_sample((MS_PER_YEAR_F / 2.0) as i64, 1000.0);
        assert!((a.lines.idle_cash_cost - 25.0).abs() < 1e-6);
        a.on_cash_sample(MS_PER_YEAR_F as i64, -50.0);
        assert!((a.lines.idle_cash_cost - 25.0).abs() < 1e-6, "negative cash accrues nothing");
    }

    #[test]
    fn regimes_split_crash_rally_and_range() {
        let mut daily = Vec::new();
        let mut spot = 100.0;
        for i in 0..120 {
            spot *= if i < 40 { 0.99 } else if i < 80 { 1.012 } else { 1.0 };
            daily.push(DailyAttr {
                ts_ms: i as i64 * MS_PER_DAY, spot, nav: 1.0, cash: 1.0, option_marks: 0.0, perp_unrealized: 0.0, equity_flows: 0.0, lines: Default::default(), attr: Default::default(),
                fills: 0, fills_call: 0, fills_put: 0,
            });
        }
        let l = regime_labels(&daily);
        assert!(l[39].starts_with("crash"), "{}", l[39]);
        assert!(l[79].starts_with("rally"), "{}", l[79]);
        assert!(l[119].starts_with("range/low_vol"), "{}", l[119]);
        let w = by_regime(&daily, 0.03);
        assert!(w.len() >= 3 && w.len() <= 6, "{:?}", w.iter().map(|w| &w.label).collect::<Vec<_>>());
        let months = month_boundaries(&daily);
        assert_eq!(months.len(), 4);
        let turns = by_boundaries(&daily, 0.03, &months, "month-");
        assert_eq!(turns.len(), 4);
        assert!((turns.iter().map(|t| t.days).sum::<f64>() - 119.0).abs() < 1e-9);
    }
}

/// The full attribution report of one run: cumulative, by option type,
/// by regime, by turn (fill-to-fill under `per_turn` flow, calendar
/// months otherwise), and the reconciliation identities.
#[derive(Clone, Debug, Serialize)]
pub struct AttributionReport {
    pub cumulative: Window,
    pub by_type: Vec<TypeView>,
    pub by_regime: Vec<Window>,
    pub by_turn: Vec<Window>,
    pub turn_basis: &'static str,
    pub lines: AttrLines,
    /// `ledger (spread + option_mark + option_exit) − per-type (edge + mtm + exit)`: zero by construction.
    pub option_identity_gap: f64,
    /// `perp_pnl − (delta + basis − slippage + residual)`: zero by construction.
    pub perp_identity_gap: f64,
    pub explanation_cadence_min: i64,
    pub note_model_edge: &'static str,
}

pub fn report(s: &crate::scenario::Scenario, out: &crate::engine::RunOutput) -> Option<AttributionReport> {
    let daily = &out.daily;
    if daily.len() < 2 {
        return None;
    }
    let fee = s.hedge.fixed_fee_per_fill;
    let first = daily.first().expect("len >= 2");
    let last = daily.last().expect("len >= 2");
    let cumulative = window("cumulative", first, last, fee);
    let per_turn = s.flow.source == "constant" && s.flow.mode == "per_turn";
    let (bounds, basis, prefix): (Vec<i64>, &'static str, &str) = if per_turn {
        (out.fill_ms.clone(), "fill_to_fill(per_turn)", "turn-")
    } else {
        (month_boundaries(daily), "calendar_month", "month-")
    };
    let by_turn = by_boundaries(daily, fee, &bounds, prefix);
    let by_regime = by_regime(daily, fee);
    let l = out.attribution;
    let lines = &out.ledger.lines;
    let mut by_type = Vec::new();
    for is_put in [false, true] {
        let settled: Vec<&crate::engine::SettledOption> = out.settled.iter().filter(|o| o.is_put == is_put).collect();
        let n = settled.len().max(1) as f64;
        let paths: BTreeMap<String, u64> = out
            .exercise
            .paths
            .iter()
            .filter(|(k, _)| if is_put { !k.starts_with("call") } else { k.starts_with("call") })
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        by_type.push(TypeView {
            option_type: if is_put { "put" } else { "call" },
            fills: if is_put { out.stats.fills_put } else { out.stats.fills_call },
            settled: settled.len(),
            premium_paid: settled.iter().map(|o| o.premium_paid).sum(),
            option_payoff: settled.iter().map(|o| o.payoff).sum(),
            option_leg_pnl: settled.iter().map(|o| o.option_leg_pnl).sum(),
            model_edge_at_entry_non_realized: if is_put { l.model_edge_put } else { l.model_edge_call },
            option_mtm: if is_put { l.option_mtm_put } else { l.option_mtm_call },
            exit_vs_mark: if is_put { l.exit_vs_mark_put } else { l.exit_vs_mark_call },
            mean_sigma_paid: settled.iter().map(|o| o.sigma_paid).sum::<f64>() / n,
            mean_sigma_realized: settled.iter().map(|o| o.sigma_realized).sum::<f64>() / n,
            exercise_paths: paths,
            note: "premium/payoff over SETTLED positions of this type; the hedge is one netted book and is reported at book level",
        });
    }
    // The per-type split must sum back to the ledger's own lines.
    let option_identity_gap = (lines.spread + lines.option_mark + lines.option_exit) - (l.model_edge() + l.option_mtm() + l.exit_vs_mark());
    let perp_identity_gap = l.perp_pnl - (l.perp_delta + l.perp_basis - lines.hedge_slippage + l.perp_residual);
    Some(AttributionReport {
        cumulative,
        by_type,
        by_regime,
        by_turn,
        turn_basis: basis,
        lines: l,
        option_identity_gap,
        perp_identity_gap,
        explanation_cadence_min: s.attribution_interval_min,
        note_model_edge: "model edge at entry is the desk's own fair value minus the bid paid; it is NOT realized revenue (doc 08 §2.3, §12 item 15)",
    })
}
