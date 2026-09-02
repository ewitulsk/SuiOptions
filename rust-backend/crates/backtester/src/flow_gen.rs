//! Earn flow generator (doc 08 §8.2–§8.3, PR N): separate call and put
//! arrival models conditioned on trailing return and direction, vol
//! regime, the displayed strike/tenor menu, moneyness, displayed
//! writer-net premium APY, collateral type / alternative yield, time of
//! day and the expiry calendar; heavy-tailed sizes bounded by protocol
//! limits; joint type/strike/expiry selection with synchronized bucket
//! concentration ("herding").
//!
//! Every parameter here is a STATED PRIOR (doc 08 §3.1, decided
//! 2026-09-01): no own-exchange or testnet RFQ data calibrates it and
//! `desk_rfq_outcomes` is never read. Outputs carry [`PRIOR_LABEL`].
//!
//! Two sources implement [`FlowSource`]: the v0 [`ConstantSource`]
//! (doc 09 G4's injector, kept as `flow.source = "constant"`) and the
//! generator [`FlowGen`] (`"generated"`) in `market` or `capacity` mode.
//! Both request only specs the Earn page would display: strikes on the
//! live lattice and expiries on the live board (doc 09 G13).

use std::collections::VecDeque;

use anyhow::Result;
use pricing::grid::{expiry_board, lattice_strikes};
use serde::Serialize;

use crate::flow::rfqs_for;
use crate::rng::{poisson_inverse, Pcg32};
use crate::scenario::{FlowConfig, FlowGenConfig, TypePriors};
use crate::{MS_PER_DAY, MS_PER_YEAR_F};

/// The provenance label every arrival/acceptance parameter carries.
pub const PRIOR_LABEL: &str = "prior (stated, uncalibrated: doc 08 §3.1 2026-09-01)";

const TAG_ARRIVAL: u64 = 0x41;
const TAG_RFQ: u64 = 0x52;
const TAG_SCHEDULE: u64 = 0x53;

/// Common-random-numbers identity of one RFQ: "the k-th `is_put` RFQ
/// arriving in minute `minute`". Everything drawn for it is keyed on
/// this, so parameter variants see the same writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct RfqKey {
    pub minute: i64,
    pub is_put: bool,
    pub k: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct RfqEvent {
    pub key: RfqKey,
    pub arrival_ms: i64,
    pub is_put: bool,
    pub strike: f64,
    pub expiry_ms: i64,
    /// Underlying units.
    pub qty: f64,
    /// Underlying spot notional at arrival (doc 08 §8.1 "offered").
    pub offered_notional: f64,
    /// The writer's alternative yield on the collateral: staking for a
    /// call (underlying collateral), settlement lending for a put.
    pub alt_yield: f64,
}

/// What the engine exposes to a flow source each minute.
#[derive(Clone, Copy, Debug)]
pub struct FlowCtx {
    pub now_ms: i64,
    pub spot: f64,
    /// ATM surface sigma at the scenario's reference tenor.
    pub sigma_atm: f64,
    pub nav: f64,
    pub stale: bool,
    /// Displayed writer-net premium APY of the indicative call/put menu
    /// entry (None while the desk has no fresh price).
    pub apy_call: Option<f64>,
    pub apy_put: Option<f64>,
}

pub trait FlowSource {
    fn rfqs(&mut self, ctx: &FlowCtx) -> Vec<RfqEvent>;
    /// `(is_put, strike, expiry_ms)` specs whose indicative bid sets the
    /// displayed APY the arrival model reads. Empty = not needed.
    fn indicative_specs(&self, _ctx: &FlowCtx) -> Vec<(bool, f64, i64)> {
        Vec::new()
    }
    fn label(&self) -> &'static str;
    fn turns(&self) -> u64 {
        0
    }
    fn stale_declines(&self) -> u64 {
        0
    }
}

/// The v0 constant injector behind the trait: `per_turn` / `daily`
/// notional at a fixed mix and tenor. A turn or day that lands on a
/// stale price is retried every minute (declined, never skipped).
pub struct ConstantSource {
    cfg: FlowConfig,
    next_turn_ms: i64,
    next_daily_ms: i64,
    tenor_ms: i64,
    turns: u64,
    stale: u64,
}

impl ConstantSource {
    pub fn new(cfg: &FlowConfig, start_ms: i64) -> Result<Self> {
        anyhow::ensure!(matches!(cfg.mode.as_str(), "per_turn" | "daily"), "unknown flow.mode {}", cfg.mode);
        Ok(Self {
            cfg: cfg.clone(),
            next_turn_ms: start_ms,
            next_daily_ms: start_ms + cfg.hour_utc as i64 * 3_600_000,
            tenor_ms: (cfg.tenor_days * MS_PER_DAY as f64) as i64,
            turns: 0,
            stale: 0,
        })
    }
}

impl FlowSource for ConstantSource {
    fn rfqs(&mut self, ctx: &FlowCtx) -> Vec<RfqEvent> {
        let due = match self.cfg.mode.as_str() {
            "per_turn" => ctx.now_ms >= self.next_turn_ms,
            _ => ctx.now_ms >= self.next_daily_ms,
        };
        if !due {
            return Vec::new();
        }
        if ctx.stale {
            self.stale += 1;
            return Vec::new();
        }
        let notional = if self.cfg.mode == "per_turn" {
            self.next_turn_ms = ctx.now_ms + self.tenor_ms;
            self.turns += 1;
            self.cfg.notional_nav_multiple * ctx.nav
        } else {
            self.next_daily_ms += MS_PER_DAY;
            self.cfg.notional_per_day
        };
        rfqs_for(&self.cfg, ctx.now_ms, ctx.spot, ctx.sigma_atm, notional)
            .into_iter()
            .map(|r| RfqEvent {
                key: RfqKey { minute: ctx.now_ms / 60_000, is_put: r.is_put, k: 0 },
                arrival_ms: ctx.now_ms,
                is_put: r.is_put,
                strike: r.strike,
                expiry_ms: r.expiry_ms,
                qty: r.qty,
                offered_notional: r.qty * ctx.spot,
                alt_yield: 0.0,
            })
            .collect()
    }

    fn label(&self) -> &'static str {
        "constant"
    }

    fn turns(&self) -> u64 {
        self.turns
    }

    fn stale_declines(&self) -> u64 {
        self.stale
    }
}

/// The conditioning features of one arrival-intensity evaluation.
#[derive(Clone, Copy, Debug, Default)]
pub struct Features {
    /// ln(spot / spot one trailing window ago).
    pub trailing_return: f64,
    /// ln(sigma / sigma one trailing window ago): a vol spike.
    pub vol_spike: f64,
    /// ln(sigma / reference vol): the vol regime level.
    pub vol_level: f64,
    /// Displayed writer-net APY for this type, if the desk is quoting.
    pub apy: Option<f64>,
    pub hour_utc: f64,
    /// Hours since the most recent board expiry (writers roll after it).
    pub hours_since_expiry: f64,
}

/// Arrivals per day for one option type under the stated priors:
///
/// ```text
/// λ = base × exp(β_ret·r + β_spike·Δlnσ + β_level·lnσ/σ_ref)
///        × (APY/APY_ref)^ε × [APY < alt_yield ? penalty : 1]
///        × (1 + A·cos(2π(h − peak)/24)) × (1 + calendar boost)
/// ```
pub fn intensity_per_day(cfg: &FlowGenConfig, p: &TypePriors, f: &Features) -> f64 {
    let regime = (p.return_coef * f.trailing_return + p.vol_spike_coef * f.vol_spike + p.vol_level_coef * f.vol_level).exp();
    let apy_mult = match f.apy {
        Some(apy) => {
            let elastic = (apy.max(1e-4) / p.apy_ref.max(1e-4)).powf(p.apy_elasticity);
            let alt = if apy < p.alt_yield { p.alt_yield_penalty } else { 1.0 };
            elastic * alt
        }
        None => 1.0,
    };
    let tod = (1.0 + cfg.tod_amplitude * (std::f64::consts::TAU * (f.hour_utc - cfg.tod_peak_hour) / 24.0).cos()).max(0.0);
    let cal = if f.hours_since_expiry >= 0.0 && f.hours_since_expiry < cfg.calendar_window_hours { 1.0 + cfg.calendar_boost } else { 1.0 };
    p.base_rate_per_day * regime * apy_mult * tod * cal
}

/// The generator. `mode = "market"`: Poisson arrivals at the conditioned
/// intensity (elastic demand). `mode = "capacity"`: a fixed count per
/// day with heavy-tailed sizes rescaled so the day's offered notional
/// equals the target — demand-inelastic injection for the solver.
pub struct FlowGen {
    cfg: FlowGenConfig,
    tick_pct: f64,
    z_width: f64,
    seed: u64,
    spot_hist: VecDeque<(i64, f64)>,
    sigma_hist: VecDeque<(i64, f64)>,
    /// Last chosen (strike, expiry) per type — the herding target.
    hot: [Option<(f64, i64)>; 2],
    queue: VecDeque<RfqEvent>,
    scheduled_day: i64,
    last_board_expiry: i64,
}

impl FlowGen {
    pub fn new(cfg: &FlowGenConfig, flow: &FlowConfig, seed: u64) -> Result<Self> {
        anyhow::ensure!(matches!(cfg.mode.as_str(), "market" | "capacity"), "unknown flow_gen.mode {}", cfg.mode);
        anyhow::ensure!((0.0..=1.0).contains(&cfg.call_share), "flow_gen.call_share in [0,1]");
        anyhow::ensure!(cfg.use_expiry_board || !cfg.tenor_menu_days.is_empty(), "flow_gen.tenor_menu_days empty");
        Ok(Self {
            cfg: cfg.clone(),
            tick_pct: flow.tick_pct,
            z_width: flow.z_width,
            seed,
            spot_hist: VecDeque::new(),
            sigma_hist: VecDeque::new(),
            hot: [None, None],
            queue: VecDeque::new(),
            scheduled_day: i64::MIN,
            last_board_expiry: i64::MIN,
        })
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    fn priors(&self, is_put: bool) -> &TypePriors {
        if is_put { &self.cfg.put } else { &self.cfg.call }
    }

    fn observe(&mut self, ctx: &FlowCtx) {
        let window = (self.cfg.trailing_window_hours * 3_600_000.0) as i64;
        self.spot_hist.push_back((ctx.now_ms, ctx.spot));
        self.sigma_hist.push_back((ctx.now_ms, ctx.sigma_atm));
        while self.spot_hist.front().is_some_and(|(t, _)| ctx.now_ms - *t > window) {
            self.spot_hist.pop_front();
        }
        while self.sigma_hist.front().is_some_and(|(t, _)| ctx.now_ms - *t > window) {
            self.sigma_hist.pop_front();
        }
        // The board's most recent expiry: the active weekly rolled over.
        let active = expiry_board(ctx.now_ms)[0];
        let prev = active - pricing::grid::WEEK_MS;
        if prev <= ctx.now_ms {
            self.last_board_expiry = self.last_board_expiry.max(prev);
        }
    }

    pub fn features(&self, ctx: &FlowCtx, is_put: bool) -> Features {
        let trailing_return = self.spot_hist.front().map(|(_, s0)| (ctx.spot / s0).ln()).unwrap_or(0.0);
        let vol_spike = self.sigma_hist.front().map(|(_, v0)| (ctx.sigma_atm / v0.max(1e-6)).ln()).unwrap_or(0.0);
        let vol_level = (ctx.sigma_atm.max(1e-6) / self.cfg.reference_vol.max(1e-6)).ln();
        let hour_utc = (ctx.now_ms.rem_euclid(MS_PER_DAY)) as f64 / 3_600_000.0;
        let hours_since_expiry = if self.last_board_expiry == i64::MIN { f64::INFINITY } else { (ctx.now_ms - self.last_board_expiry) as f64 / 3_600_000.0 };
        Features {
            trailing_return,
            vol_spike,
            vol_level,
            apy: if is_put { ctx.apy_put } else { ctx.apy_call },
            hour_utc,
            hours_since_expiry,
        }
    }

    /// Expiry menu at `now`: the live board (entries at least
    /// `min_tenor_days` out) or the configured tenor menu.
    fn expiry_menu(&self, now_ms: i64) -> Vec<i64> {
        let min_ms = (self.cfg.min_tenor_days * MS_PER_DAY as f64) as i64;
        if self.cfg.use_expiry_board {
            let board: Vec<i64> = expiry_board(now_ms).into_iter().filter(|e| e - now_ms >= min_ms).collect();
            if !board.is_empty() {
                return board;
            }
        }
        self.cfg.tenor_menu_days.iter().map(|d| now_ms + (d * MS_PER_DAY as f64) as i64).collect()
    }

    /// Geometric concentration on the nearest listed expiries: the
    /// nearest gets `expiry_concentration` of the mass, the next the same
    /// fraction of the remainder, and so on.
    fn pick_expiry(&self, menu: &[i64], u: f64) -> i64 {
        let c = self.cfg.expiry_concentration.clamp(0.0, 1.0);
        let n = menu.len();
        let mut weights: Vec<f64> = (0..n).map(|i| c * (1.0 - c).powi(i as i32)).collect();
        if let Some(last) = weights.last_mut() {
            // Give the tail its full remaining mass so the weights sum to 1.
            *last = (1.0 - c).powi((n - 1) as i32);
        }
        let mut acc = 0.0;
        for (i, w) in weights.iter().enumerate() {
            acc += w;
            if u <= acc {
                return menu[i];
            }
        }
        menu[n - 1]
    }

    fn strike_for(&self, spot: f64, sigma: f64, expiry_ms: i64, now_ms: i64, z: f64) -> f64 {
        let tau = ((expiry_ms - now_ms) as f64 / MS_PER_YEAR_F).max(1e-6);
        let sigma = sigma.max(1e-3);
        let target = spot * (z * sigma * tau.sqrt()).exp();
        lattice_strikes(spot, sigma, tau, self.tick_pct, self.z_width)
            .into_iter()
            .min_by(|a, b| (a - target).abs().partial_cmp(&(b - target).abs()).unwrap())
            .unwrap_or(target)
    }

    /// One writer: size, then (herded or fresh) bucket.
    fn draw_rfq(&mut self, key: RfqKey, now_ms: i64, spot: f64, sigma: f64, notional_override: Option<f64>) -> RfqEvent {
        let p = *self.priors(key.is_put);
        let mut rng = Pcg32::keyed(self.seed, &[key.minute as u64, key.is_put as u64, key.k as u64, TAG_RFQ]);
        let raw = rng.lognormal(p.size_median, p.size_log_sd);
        let u_herd = rng.uniform();
        let u_exp = rng.uniform();
        let z = p.moneyness_mean_z + p.moneyness_sd_z * rng.normal();
        let notional = notional_override.unwrap_or(raw).clamp(self.cfg.min_notional, self.cfg.max_notional);
        let slot = key.is_put as usize;
        let herd = self.hot[slot].filter(|(_, e)| *e > now_ms + (self.cfg.min_tenor_days * MS_PER_DAY as f64) as i64);
        let (strike, expiry_ms) = match herd {
            Some(h) if u_herd < self.cfg.herd_prob => h,
            _ => {
                let menu = self.expiry_menu(now_ms);
                let e = self.pick_expiry(&menu, u_exp);
                (self.strike_for(spot, sigma, e, now_ms, z), e)
            }
        };
        self.hot[slot] = Some((strike, expiry_ms));
        RfqEvent {
            key,
            arrival_ms: now_ms,
            is_put: key.is_put,
            strike,
            expiry_ms,
            qty: notional / spot,
            offered_notional: notional,
            alt_yield: p.alt_yield,
        }
    }

    /// Raw (unclipped) size draw for a key — what capacity mode rescales.
    fn raw_size(&self, key: RfqKey) -> f64 {
        let p = self.priors(key.is_put);
        let mut rng = Pcg32::keyed(self.seed, &[key.minute as u64, key.is_put as u64, key.k as u64, TAG_RFQ]);
        rng.lognormal(p.size_median, p.size_log_sd)
    }

    /// Capacity mode: lay out one day of arrivals up front so the day's
    /// offered notional per type is exactly the target share.
    fn schedule_day(&mut self, day: i64, ctx: &FlowCtx) {
        let day_start = day * MS_PER_DAY;
        let n = self.cfg.rfqs_per_day.max(1);
        let n_call = (n as f64 * self.cfg.call_share).round() as u32;
        let n_put = n - n_call.min(n);
        let mut rng = Pcg32::keyed(self.seed, &[day as u64, TAG_SCHEDULE]);
        for (is_put, count, share) in [(false, n_call, self.cfg.call_share), (true, n_put, 1.0 - self.cfg.call_share)] {
            if count == 0 || share <= 0.0 {
                continue;
            }
            let mut minutes: Vec<i64> = (0..count).map(|_| (rng.uniform() * 1440.0) as i64).collect();
            minutes.sort_unstable();
            let mut keys = Vec::with_capacity(count as usize);
            let mut last = (i64::MIN, 0u32);
            for m in minutes {
                let k = if last.0 == m { last.1 + 1 } else { 0 };
                last = (m, k);
                keys.push(RfqKey { minute: day_start / 60_000 + m, is_put, k });
            }
            let raw: Vec<f64> = keys.iter().map(|k| self.raw_size(*k)).collect();
            let sum: f64 = raw.iter().sum();
            let target = self.cfg.target_notional_per_day * share;
            let scale = if sum > 0.0 { target / sum } else { 0.0 };
            for (key, r) in keys.into_iter().zip(raw) {
                let now_ms = key.minute * 60_000;
                let ev = self.draw_rfq(key, now_ms, ctx.spot, ctx.sigma_atm, Some(r * scale));
                self.queue.push_back(ev);
            }
        }
        // Arrival order across types.
        let mut v: Vec<RfqEvent> = self.queue.drain(..).collect();
        v.sort_by_key(|e| (e.arrival_ms, e.is_put, e.key.k));
        self.queue.extend(v);
    }
}

impl FlowSource for FlowGen {
    fn rfqs(&mut self, ctx: &FlowCtx) -> Vec<RfqEvent> {
        self.observe(ctx);
        let minute = ctx.now_ms / 60_000;
        if self.cfg.mode == "capacity" {
            let day = ctx.now_ms.div_euclid(MS_PER_DAY);
            if day != self.scheduled_day {
                self.scheduled_day = day;
                self.queue.retain(|e| e.arrival_ms >= ctx.now_ms);
                self.schedule_day(day, ctx);
            }
            let mut out = Vec::new();
            while self.queue.front().is_some_and(|e| e.arrival_ms <= ctx.now_ms) {
                let mut e = self.queue.pop_front().unwrap();
                // Sizes were fixed at scheduling; the bucket is re-drawn at
                // arrival so it sits on the lattice/board of the moment.
                let fresh = self.draw_rfq(e.key, ctx.now_ms, ctx.spot, ctx.sigma_atm, Some(e.offered_notional));
                e.strike = fresh.strike;
                e.expiry_ms = fresh.expiry_ms;
                e.qty = e.offered_notional / ctx.spot;
                e.arrival_ms = ctx.now_ms;
                out.push(e);
            }
            return out;
        }
        let mut out = Vec::new();
        for is_put in [false, true] {
            let f = self.features(ctx, is_put);
            let lam = intensity_per_day(&self.cfg, self.priors(is_put), &f) / 1440.0;
            let u = Pcg32::keyed(self.seed, &[minute as u64, is_put as u64, TAG_ARRIVAL]).uniform();
            let n = poisson_inverse(u, lam).min(self.cfg.max_rfqs_per_minute);
            for k in 0..n {
                let key = RfqKey { minute, is_put, k };
                out.push(self.draw_rfq(key, ctx.now_ms, ctx.spot, ctx.sigma_atm, None));
            }
        }
        out
    }

    fn indicative_specs(&self, ctx: &FlowCtx) -> Vec<(bool, f64, i64)> {
        if self.cfg.mode != "market" {
            return Vec::new();
        }
        let menu = self.expiry_menu(ctx.now_ms);
        let want = ctx.now_ms + (self.cfg.apy_reference_tenor_days * MS_PER_DAY as f64) as i64;
        let expiry = menu.into_iter().min_by_key(|e| (e - want).abs()).unwrap_or(want);
        [false, true]
            .into_iter()
            .map(|is_put| (is_put, self.strike_for(ctx.spot, ctx.sigma_atm, expiry, ctx.now_ms, self.priors(is_put).moneyness_mean_z), expiry))
            .collect()
    }

    fn label(&self) -> &'static str {
        if self.cfg.mode == "capacity" { "generated_capacity" } else { "generated_market" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(now_ms: i64, spot: f64) -> FlowCtx {
        FlowCtx { now_ms, spot, sigma_atm: 0.8, nav: 1e6, stale: false, apy_call: Some(1.0), apy_put: Some(0.8) }
    }

    fn drain(g: &mut FlowGen, days: i64, spot: impl Fn(i64) -> f64) -> Vec<RfqEvent> {
        let mut out = Vec::new();
        for m in 0..days * 1440 {
            let now = m * 60_000;
            out.extend(g.rfqs(&ctx(now, spot(now))));
        }
        out
    }

    #[test]
    fn call_and_put_models_respond_differently_to_return_direction_and_apy() {
        let cfg = FlowGenConfig::default();
        let up = Features { trailing_return: 0.10, apy: Some(1.0), ..Default::default() };
        let dn = Features { trailing_return: -0.10, apy: Some(1.0), ..Default::default() };
        let c_up = intensity_per_day(&cfg, &cfg.call, &up);
        let c_dn = intensity_per_day(&cfg, &cfg.call, &dn);
        let p_up = intensity_per_day(&cfg, &cfg.put, &up);
        let p_dn = intensity_per_day(&cfg, &cfg.put, &dn);
        assert!(c_up > c_dn, "calls should rise after a run-up: {c_up} vs {c_dn}");
        assert!(p_dn > p_up, "puts should rise after a sell-off: {p_dn} vs {p_up}");
        // Vol spike lifts puts more than calls.
        let spike = Features { vol_spike: 0.5, apy: Some(1.0), ..Default::default() };
        let base = Features { apy: Some(1.0), ..Default::default() };
        let c_ratio = intensity_per_day(&cfg, &cfg.call, &spike) / intensity_per_day(&cfg, &cfg.call, &base);
        let p_ratio = intensity_per_day(&cfg, &cfg.put, &spike) / intensity_per_day(&cfg, &cfg.put, &base);
        assert!(p_ratio > c_ratio, "{p_ratio} vs {c_ratio}");
        // APY: both rise with displayed APY, at different elasticities.
        let lo = Features { apy: Some(0.4), ..Default::default() };
        let hi = Features { apy: Some(1.6), ..Default::default() };
        let c_el = (intensity_per_day(&cfg, &cfg.call, &hi) / intensity_per_day(&cfg, &cfg.call, &lo)).ln() / (4.0f64).ln();
        let p_el = (intensity_per_day(&cfg, &cfg.put, &hi) / intensity_per_day(&cfg, &cfg.put, &lo)).ln() / (4.0f64).ln();
        assert!(c_el > 0.0 && p_el > 0.0);
        assert!((c_el - p_el).abs() > 0.1, "elasticities should differ: {c_el} vs {p_el}");
        // Below the collateral's alternative yield the writer stays away.
        let under = Features { apy: Some(0.02), ..Default::default() };
        assert!(intensity_per_day(&cfg, &cfg.call, &under) < intensity_per_day(&cfg, &cfg.call, &lo));
    }

    #[test]
    fn same_seed_identical_flow_and_specs_on_the_live_menu() {
        let cfg = FlowGenConfig::default();
        let flow = FlowConfig::default();
        let mut a = FlowGen::new(&cfg, &flow, 7).unwrap();
        let mut b = FlowGen::new(&cfg, &flow, 7).unwrap();
        let mut c = FlowGen::new(&cfg, &flow, 8).unwrap();
        let path = |t: i64| 3.0 + 0.2 * ((t as f64) / 8.64e7).sin();
        let ea = drain(&mut a, 3, path);
        let eb = drain(&mut b, 3, path);
        let ec = drain(&mut c, 3, path);
        assert!(!ea.is_empty());
        assert_eq!(ea, eb, "same seed must give identical flow");
        assert_ne!(ea.len(), ec.len());
        assert!(ea.iter().any(|e| e.is_put) && ea.iter().any(|e| !e.is_put));
        for e in &ea {
            let tau = (e.expiry_ms - e.arrival_ms) as f64 / MS_PER_YEAR_F;
            let spot = path(e.arrival_ms);
            let ladder = lattice_strikes(spot, 0.8, tau, flow.tick_pct, flow.z_width);
            assert!(ladder.iter().any(|k| (k - e.strike).abs() < 1e-9), "strike {} off lattice", e.strike);
            assert!(expiry_board(e.arrival_ms).contains(&e.expiry_ms), "expiry off board");
            assert!(e.offered_notional >= cfg.min_notional && e.offered_notional <= cfg.max_notional);
        }
        // Heavy tail: the largest size is many times the median.
        let mut sizes: Vec<f64> = ea.iter().map(|e| e.offered_notional).collect();
        sizes.sort_by(|x, y| x.partial_cmp(y).unwrap());
        let med = sizes[sizes.len() / 2];
        assert!(sizes[sizes.len() - 1] > 5.0 * med, "tail {} vs median {med}", sizes[sizes.len() - 1]);
    }

    #[test]
    fn common_random_numbers_hold_across_variants() {
        let flow = FlowConfig::default();
        let base = FlowGenConfig::default();
        let mut wider = base.clone();
        // A wider bid shows a lower APY: fewer arrivals, same writers.
        let mut a = FlowGen::new(&base, &flow, 3).unwrap();
        let mut b = FlowGen::new(&wider, &flow, 3).unwrap();
        wider.call.base_rate_per_day *= 2.0;
        let mut c = FlowGen::new(&wider, &flow, 3).unwrap();
        let mut ea = Vec::new();
        let mut eb = Vec::new();
        let mut ec = Vec::new();
        for m in 0..2 * 1440 {
            let mut hi = ctx(m * 60_000, 3.0);
            let mut lo = hi;
            lo.apy_call = Some(0.5);
            lo.apy_put = Some(0.4);
            ea.extend(a.rfqs(&hi));
            eb.extend(b.rfqs(&lo));
            hi.apy_call = Some(1.0);
            ec.extend(c.rfqs(&hi));
        }
        assert!(eb.len() < ea.len(), "lower APY must reduce arrivals: {} vs {}", eb.len(), ea.len());
        assert!(ec.len() > ea.len());
        // Every RFQ key present in the low-APY variant is the same writer
        // (size) as in the base; the base is a superset (monotone Poisson).
        for e in &eb {
            let twin = ea.iter().find(|x| x.key == e.key).expect("CRN: low-APY arrivals ⊂ base arrivals");
            assert_eq!(twin.offered_notional, e.offered_notional);
        }
        for e in &ea {
            let twin = ec.iter().find(|x| x.key == e.key).expect("CRN: base ⊂ doubled-rate");
            assert_eq!(twin.offered_notional, e.offered_notional);
        }
    }

    #[test]
    fn capacity_mode_injects_the_target_exactly_and_doubles_with_it() {
        let flow = FlowConfig::default();
        let mut cfg = FlowGenConfig { mode: "capacity".into(), target_notional_per_day: 50_000.0, call_share: 0.5, rfqs_per_day: 10, min_notional: 0.0, max_notional: 1e9, ..Default::default() };
        let mut g = FlowGen::new(&cfg, &flow, 1).unwrap();
        let ev = drain(&mut g, 2, |_| 3.0);
        assert_eq!(ev.len(), 20);
        let day0: f64 = ev.iter().filter(|e| e.arrival_ms < MS_PER_DAY).map(|e| e.offered_notional).sum();
        assert!((day0 - 50_000.0).abs() < 1e-6, "{day0}");
        let calls: f64 = ev.iter().filter(|e| !e.is_put && e.arrival_ms < MS_PER_DAY).map(|e| e.offered_notional).sum();
        assert!((calls - 25_000.0).abs() < 1e-6);
        cfg.target_notional_per_day = 100_000.0;
        let mut g2 = FlowGen::new(&cfg, &flow, 1).unwrap();
        let ev2 = drain(&mut g2, 2, |_| 3.0);
        assert_eq!(ev2.len(), ev.len());
        for (a, b) in ev.iter().zip(&ev2) {
            assert_eq!(a.key, b.key);
            assert!((b.offered_notional - 2.0 * a.offered_notional).abs() < 1e-6);
        }
    }

    #[test]
    fn herding_concentrates_buckets() {
        let flow = FlowConfig::default();
        let all = FlowGenConfig { herd_prob: 1.0, ..Default::default() };
        let none = FlowGenConfig { herd_prob: 0.0, ..Default::default() };
        let mut a = FlowGen::new(&all, &flow, 5).unwrap();
        let mut b = FlowGen::new(&none, &flow, 5).unwrap();
        let ea = drain(&mut a, 2, |_| 3.0);
        let eb = drain(&mut b, 2, |_| 3.0);
        let distinct = |v: &[RfqEvent]| {
            let mut s: Vec<(bool, i64, i64)> = v.iter().map(|e| (e.is_put, (e.strike * 1e6) as i64, e.expiry_ms)).collect();
            s.sort_unstable();
            s.dedup();
            s.len()
        };
        assert!(distinct(&ea) < distinct(&eb), "{} vs {}", distinct(&ea), distinct(&eb));
    }
}
