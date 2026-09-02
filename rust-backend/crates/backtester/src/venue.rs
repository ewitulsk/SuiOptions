//! The simulated hedge-venue seam. The order/event types mirror
//! `services/mm-bot/src/desk/hedge.rs` (`HedgeOrder`, `HedgeCommand`,
//! `Fill`, `HedgeEvent`, `OpenOrders`) one to one — commands in, events
//! out, signed sizes, fill-after-cancel resolved in the venue's favour —
//! so the engine drives a venue exactly the way the desk's rebalancer
//! drives a live one. They are mirrored rather than imported because
//! `mm-bot` links the Sui SDK; a shared `desk-core` (doc 08 §5.2) is
//! where both will come from.
//!
//! `SimPerpVenue` is the doc 08 §7.2/§7.3 lifecycle: taker fills against
//! the bar path with slippage, persistent own-order impact and a depth cap
//! (partial fills), passive placement at the desk's reference price with
//! queue-ahead assumptions, cancel latency and fill-after-cancel races,
//! contract rounding, maker/taker/fixed fees, venue outages, and the
//! Bluefin isolated-margin liquidation rule from `margin.rs` checked at
//! every mark. Every result names one execution assumption:
//! `optimistic | central | conservative | taker_only`. Passive results in
//! the proxy-BBO era are sensitivity only, never calibrated fact.

use std::collections::BTreeMap;

use crate::latency::{LatencyModel, LatencyStage};
use crate::margin::{IsolatedPosition, MarginConfig};
use crate::scenario::VenueConfig;

pub type OrderId = u64;

/// One hedge order: signed size in underlying units (positive buys).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HedgeOrder {
    pub id: OrderId,
    pub size_units: f64,
    /// Reference spot the desk priced the order at.
    pub spot: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HedgeCommand {
    Submit(HedgeOrder),
    Cancel(OrderId),
    Replace { old: OrderId, new: HedgeOrder },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fill {
    pub order: OrderId,
    /// Signed size filled (same sign convention as the order).
    pub size_units: f64,
    pub price: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HedgeEvent {
    Acknowledged(OrderId),
    PartiallyFilled(Fill),
    Filled(Fill),
    Rejected { order: OrderId, reason: String },
    Cancelled(OrderId),
}

/// How the desk wants a simulated order worked (the live seam has no
/// order type: the venue adapter decides; here the engine's policy does).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderKind {
    Taker,
    Passive,
}

/// What comes back from the venue.
#[derive(Clone, Debug, PartialEq)]
pub enum VenueEvent {
    Hedge(HedgeEvent),
    /// The risk engine closed (part of) the position at `price`;
    /// `penalty` is what the trader forfeits on top of the mark P&L.
    Liquidated { size_closed: f64, price: f64, penalty: f64, full: bool },
    /// A margin transfer landed (or was refused by an outage).
    TopUp { amount: f64, accepted: bool },
}

/// An event stamped with the instant the DESK learns of it, the fee the
/// venue charged (fills only) and the mark at execution (for slippage
/// attribution).
#[derive(Clone, Debug, PartialEq)]
pub struct Timed {
    pub at_ms: i64,
    pub ev: VenueEvent,
    pub fee: f64,
    pub reference: f64,
}

impl Timed {
    fn hedge(at_ms: i64, ev: HedgeEvent) -> Self {
        Self { at_ms, ev: VenueEvent::Hedge(ev), fee: 0.0, reference: 0.0 }
    }
}

/// Market truth as the venue sees it: the current bar (never the
/// decision price).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MarketState {
    pub ts_ms: i64,
    /// Bar close.
    pub spot: f64,
    pub low: f64,
    pub high: f64,
    pub volume: f64,
}

impl MarketState {
    pub fn from_bar(b: &crate::data::Bar) -> Self {
        Self { ts_ms: b.ts_ms, spot: b.close, low: b.low, high: b.high, volume: b.volume }
    }
}

/// A simulated perp venue: commands arrive after submit latency; the
/// venue answers with events stamped for the desk's clock.
pub trait SimVenue {
    fn name(&self) -> &str;
    /// The queue/fill assumption every output is labeled with.
    fn execution_assumption(&self) -> &'static str;
    /// The venue mark for this market state (spot × (1 + basis)).
    fn mark(&self, market: &MarketState) -> f64;
    /// One command, arriving at the venue at `arrival_ms`.
    fn execute(&mut self, cmd: HedgeCommand, kind: OrderKind, arrival_ms: i64, market: &MarketState, account: &IsolatedPosition, lat: &mut LatencyModel) -> Vec<Timed>;
    /// A new bar of market truth: resting orders, marks, liquidation.
    fn on_bar(&mut self, market: &MarketState, account: &IsolatedPosition, lat: &mut LatencyModel) -> Vec<Timed>;
    /// A margin transfer of `amount` arriving at `arrival_ms`.
    fn topup(&mut self, amount: f64, arrival_ms: i64) -> Vec<Timed>;
}

#[derive(Clone, Copy, Debug)]
struct Resting {
    kind: OrderKind,
    remaining: f64,
    /// Passive limit (the desk's reference price).
    limit: f64,
    /// Queue ahead still to trade before this order is reached.
    queue_ahead: f64,
    /// A cancel takes effect at this instant; bars before it still fill.
    cancel_at: Option<i64>,
}

/// The doc 08 §7.2/§7.3 venue.
pub struct SimPerpVenue {
    cfg: VenueConfig,
    margin: MarginConfig,
    slippage_bps: f64,
    taker_fee_bps: f64,
    fixed_fee: f64,
    resting: BTreeMap<OrderId, Resting>,
    impact_bps: f64,
    impact_ms: i64,
    bar_ms: i64,
    depth_used: f64,
    pub liquidations: u64,
}

impl SimPerpVenue {
    pub fn new(cfg: VenueConfig, margin: MarginConfig, slippage_bps: f64, taker_fee_bps: f64, fixed_fee: f64) -> Self {
        Self { cfg, margin, slippage_bps, taker_fee_bps, fixed_fee, resting: BTreeMap::new(), impact_bps: 0.0, impact_ms: i64::MIN, bar_ms: i64::MIN, depth_used: 0.0, liquidations: 0 }
    }

    fn round_units(&self, units: f64) -> f64 {
        let c = self.cfg.contract_size.max(1e-12);
        (units.abs() / c).floor() * c * units.signum()
    }

    fn decay_impact(&mut self, now_ms: i64) {
        if self.impact_ms != i64::MIN && now_ms > self.impact_ms && self.cfg.impact_half_life_ms > 0 {
            let dt = (now_ms - self.impact_ms) as f64 / self.cfg.impact_half_life_ms as f64;
            self.impact_bps *= 0.5f64.powf(dt);
        }
        self.impact_ms = now_ms;
    }

    /// Taker price at `mark`: slippage + own impact, capped by take protection.
    fn taker_price(&self, mark: f64, sign: f64) -> f64 {
        let bps = (self.slippage_bps + self.impact_bps).min(self.cfg.take_protection_bps);
        mark * (1.0 + sign * bps / 10_000.0)
    }

    fn taker_fee(&self, units: f64, price: f64) -> f64 {
        units.abs() * price * self.taker_fee_bps / 10_000.0 + self.fixed_fee
    }

    fn maker_fee(&self, units: f64, price: f64) -> f64 {
        units.abs() * price * self.cfg.maker_fee_bps / 10_000.0 + self.fixed_fee
    }

    /// Execute up to the remaining depth of this bar as a taker at `mark`.
    /// Returns (filled units, price).
    fn take(&mut self, want: f64, mark: f64) -> Option<(f64, f64)> {
        let mut units = want;
        if self.cfg.max_taker_units_per_bar > 0.0 {
            let left = (self.cfg.max_taker_units_per_bar - self.depth_used).max(0.0);
            units = self.round_units(want.abs().min(left)) * want.signum();
        }
        if units == 0.0 {
            return None;
        }
        let price = self.taker_price(mark, units.signum());
        self.depth_used += units.abs();
        self.impact_bps += self.cfg.impact_bps_per_million * units.abs() * price / 1_000_000.0;
        Some((units, price))
    }

    fn fill_event(id: OrderId, units: f64, price: f64, remaining: f64) -> HedgeEvent {
        let f = Fill { order: id, size_units: units, price };
        if remaining.abs() <= 1e-12 {
            HedgeEvent::Filled(f)
        } else {
            HedgeEvent::PartiallyFilled(f)
        }
    }

    /// Whether the bar reached a resting order's limit under the
    /// assumption, and the bar volume eligible at that limit.
    fn eligible_volume(&self, r: &Resting, m: &MarketState) -> (bool, f64) {
        let limit = r.limit;
        let buy = r.remaining > 0.0;
        let through = self.cfg.through_bps / 10_000.0;
        let touched = if buy { m.low <= limit } else { m.high >= limit };
        let traded_through = if buy { m.low <= limit * (1.0 - through) } else { m.high >= limit * (1.0 + through) };
        let ok = if self.cfg.execution_assumption == "conservative" { traded_through } else { touched };
        if !ok {
            return (false, 0.0);
        }
        let range = m.high - m.low;
        let frac = if range <= 0.0 { 1.0 } else if buy { ((limit - m.low) / range).clamp(0.0, 1.0) } else { ((m.high - limit) / range).clamp(0.0, 1.0) };
        (true, m.volume * frac)
    }

    fn resting_fills(&mut self, m: &MarketState, lat: &mut LatencyModel) -> Vec<Timed> {
        let mut out = Vec::new();
        let mark = self.mark(m);
        let ids: Vec<OrderId> = self.resting.keys().copied().collect();
        for id in ids {
            let mut r = self.resting[&id];
            if r.cancel_at.is_some_and(|c| m.ts_ms >= c) {
                self.resting.remove(&id);
                continue;
            }
            let (units, price, fee) = match r.kind {
                OrderKind::Taker => match self.take(r.remaining, mark) {
                    Some((u, p)) => (u, p, self.taker_fee(u, p)),
                    None => continue,
                },
                OrderKind::Passive => {
                    let (reached, eligible) = self.eligible_volume(&r, m);
                    if !reached {
                        continue;
                    }
                    let units = if self.cfg.execution_assumption == "optimistic" {
                        r.remaining
                    } else {
                        let ahead = r.queue_ahead.min(eligible);
                        r.queue_ahead -= ahead;
                        let avail = (eligible - ahead) * self.cfg.passive_participation;
                        self.round_units(r.remaining.abs().min(avail)) * r.remaining.signum()
                    };
                    if units == 0.0 {
                        self.resting.insert(id, r);
                        continue;
                    }
                    (units, r.limit, self.maker_fee(units, r.limit))
                }
            };
            r.remaining -= units;
            let ev = Self::fill_event(id, units, price, r.remaining);
            if r.remaining.abs() <= 1e-12 {
                self.resting.remove(&id);
            } else {
                self.resting.insert(id, r);
            }
            out.push(Timed { at_ms: m.ts_ms + lat.draw(LatencyStage::VenueFillReport), ev: VenueEvent::Hedge(ev), fee, reference: mark });
        }
        out
    }

    fn submit(&mut self, order: HedgeOrder, kind: OrderKind, arrival_ms: i64, market: &MarketState, lat: &mut LatencyModel) -> Vec<Timed> {
        let ack_ms = arrival_ms + lat.draw(LatencyStage::VenueAck);
        let reject = |reason: &str| vec![Timed::hedge(ack_ms, HedgeEvent::Rejected { order: order.id, reason: reason.into() })];
        if self.margin.in_outage(arrival_ms) {
            return reject("venue outage");
        }
        let size = self.round_units(order.size_units);
        if order.spot <= 0.0 || size.abs() < self.cfg.min_order_units || size == 0.0 {
            return reject("below minimum order size");
        }
        let mut out = vec![Timed::hedge(ack_ms, HedgeEvent::Acknowledged(order.id))];
        let mark = self.mark(market);
        match kind {
            OrderKind::Taker => {
                if let Some((units, price)) = self.take(size, mark) {
                    let remaining = size - units;
                    let fee = self.taker_fee(units, price);
                    let ev = Self::fill_event(order.id, units, price, remaining);
                    out.push(Timed { at_ms: arrival_ms + lat.draw(LatencyStage::VenueFillReport), ev: VenueEvent::Hedge(ev), fee, reference: mark });
                    if remaining.abs() > 1e-12 {
                        self.resting.insert(order.id, Resting { kind, remaining, limit: 0.0, queue_ahead: 0.0, cancel_at: None });
                    }
                } else {
                    self.resting.insert(order.id, Resting { kind, remaining: size, limit: 0.0, queue_ahead: 0.0, cancel_at: None });
                }
            }
            OrderKind::Passive => {
                let queue_ahead = self.cfg.queue_depth_units * self.cfg.queue_ahead_mult();
                self.resting.insert(order.id, Resting { kind, remaining: size, limit: order.spot, queue_ahead, cancel_at: None });
            }
        }
        out
    }
}

impl SimVenue for SimPerpVenue {
    fn name(&self) -> &str {
        "sim-perp"
    }

    fn execution_assumption(&self) -> &'static str {
        match self.cfg.execution_assumption.as_str() {
            "optimistic" => "optimistic",
            "central" => "central",
            "conservative" => "conservative",
            _ => "taker_only",
        }
    }

    fn mark(&self, market: &MarketState) -> f64 {
        market.spot * (1.0 + self.cfg.basis_bps_at(market.ts_ms) / 10_000.0)
    }

    fn execute(&mut self, cmd: HedgeCommand, kind: OrderKind, arrival_ms: i64, market: &MarketState, _account: &IsolatedPosition, lat: &mut LatencyModel) -> Vec<Timed> {
        self.decay_impact(arrival_ms);
        match cmd {
            HedgeCommand::Submit(order) => self.submit(order, kind, arrival_ms, market, lat),
            HedgeCommand::Cancel(id) => {
                let effective = arrival_ms + lat.draw(LatencyStage::VenueCancel);
                if let Some(r) = self.resting.get_mut(&id) {
                    r.cancel_at = Some(effective);
                }
                vec![Timed::hedge(effective, HedgeEvent::Cancelled(id))]
            }
            HedgeCommand::Replace { old, new } => {
                let mut out = self.execute(HedgeCommand::Cancel(old), kind, arrival_ms, market, _account, lat);
                out.extend(self.submit(new, kind, arrival_ms, market, lat));
                out
            }
        }
    }

    fn on_bar(&mut self, market: &MarketState, account: &IsolatedPosition, lat: &mut LatencyModel) -> Vec<Timed> {
        self.decay_impact(market.ts_ms);
        if market.ts_ms != self.bar_ms {
            self.bar_ms = market.ts_ms;
            self.depth_used = 0.0;
        }
        let mut out = Vec::new();
        let mark = self.mark(market);
        // The risk engine runs even through an outage; matching does not.
        if self.margin.enabled && account.is_liquidatable(mark, self.margin.mmr) {
            self.liquidations += 1;
            let (size_closed, penalty, full) = if self.margin.partial_close > 0.0 {
                let closed = -self.round_units(account.size * self.margin.partial_close.min(1.0));
                (closed, closed.abs() * mark * self.margin.partial_penalty_bps / 10_000.0, false)
            } else {
                (-account.size, (account.margin + account.unrealized(mark)).max(0.0), true)
            };
            // Working orders are cancelled by the venue on liquidation.
            for id in self.resting.keys().copied().collect::<Vec<_>>() {
                out.push(Timed::hedge(market.ts_ms, HedgeEvent::Cancelled(id)));
            }
            self.resting.clear();
            out.push(Timed { at_ms: market.ts_ms, ev: VenueEvent::Liquidated { size_closed, price: mark, penalty, full }, fee: 0.0, reference: mark });
            return out;
        }
        if !self.margin.in_outage(market.ts_ms) {
            out.extend(self.resting_fills(market, lat));
        }
        out
    }

    fn topup(&mut self, amount: f64, arrival_ms: i64) -> Vec<Timed> {
        let accepted = !self.margin.in_outage(arrival_ms);
        vec![Timed { at_ms: arrival_ms, ev: VenueEvent::TopUp { amount, accepted }, fee: 0.0, reference: 0.0 }]
    }
}

/// One order the desk submitted that the venue has not finished resolving.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorkingOrder {
    pub size_units: f64,
    pub filled_units: f64,
    pub submitted_ms: i64,
    /// Reference spot of the order (fee/slippage attribution).
    pub spot: f64,
}

impl WorkingOrder {
    pub fn remaining_units(&self) -> f64 {
        self.size_units - self.filled_units
    }
}

/// The desk's view of its working orders (mirror of mm-bot `OpenOrders`,
/// with a BTreeMap so iteration is deterministic). A fill for an order
/// already dropped by a cancel is still returned: the venue's position
/// already reflects it.
#[derive(Debug, Default)]
pub struct OpenOrders {
    orders: BTreeMap<OrderId, WorkingOrder>,
}

impl OpenOrders {
    pub fn submit(&mut self, order: &HedgeOrder, now_ms: i64) {
        self.orders.insert(order.id, WorkingOrder { size_units: order.size_units, filled_units: 0.0, submitted_ms: now_ms, spot: order.spot });
    }

    /// Apply one event; returns the fill it carried (with the order's
    /// reference spot when the order is still known), if any.
    pub fn apply(&mut self, ev: &HedgeEvent) -> Option<(Fill, Option<f64>)> {
        match ev {
            HedgeEvent::Acknowledged(_) => None,
            HedgeEvent::PartiallyFilled(f) => {
                let mut spot = None;
                if let Some(w) = self.orders.get_mut(&f.order) {
                    w.filled_units += f.size_units;
                    spot = Some(w.spot);
                    if w.remaining_units().abs() <= 1e-12 {
                        self.orders.remove(&f.order);
                    }
                }
                Some((*f, spot))
            }
            HedgeEvent::Filled(f) => {
                let spot = self.orders.remove(&f.order).map(|w| w.spot);
                Some((*f, spot))
            }
            HedgeEvent::Rejected { order, .. } | HedgeEvent::Cancelled(order) => {
                self.orders.remove(order);
                None
            }
        }
    }

    /// Signed unfilled size across working orders.
    pub fn working_units(&self) -> f64 {
        self.orders.values().map(WorkingOrder::remaining_units).sum()
    }

    /// Orders working longer than `timeout_ms`, ascending id.
    pub fn stale(&self, now_ms: i64, timeout_ms: i64) -> Vec<OrderId> {
        self.orders.iter().filter(|(_, w)| now_ms - w.submitted_ms >= timeout_ms).map(|(id, _)| *id).collect()
    }

    pub fn len(&self) -> usize {
        self.orders.len()
    }

    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }
}

/// Mirror of mm-bot `rebalance_target`: rebalance to `−book_delta` only
/// when the net delta leaves the band.
pub fn rebalance_target(book_delta_units: f64, perp_position_units: f64, band_units: f64) -> Option<f64> {
    let net = book_delta_units + perp_position_units;
    if net.abs() > band_units {
        Some(-book_delta_units)
    } else {
        None
    }
}

/// Mirror of mm-bot `plan_hedge_order`: the signed size that brings
/// `position + working` to neutral when the net INCLUDING working orders
/// is outside the band.
pub fn plan_hedge_order(book_delta_units: f64, perp_position_units: f64, working_units: f64, band_units: f64) -> Option<f64> {
    let effective = perp_position_units + working_units;
    let target = rebalance_target(book_delta_units, effective, band_units)?;
    let size = target - effective;
    if size == 0.0 {
        None
    } else {
        Some(size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::latency::{LatencyConfig, LatencyDist};

    fn venue(cfg: VenueConfig) -> SimPerpVenue {
        SimPerpVenue::new(cfg, MarginConfig::default(), 3.5, 3.5, 0.03)
    }

    fn bar(ts: i64, low: f64, high: f64, close: f64, volume: f64) -> MarketState {
        MarketState { ts_ms: ts, spot: close, low, high, volume }
    }

    fn flat() -> IsolatedPosition {
        IsolatedPosition { size: 0.0, entry: 0.0, margin: 0.0 }
    }

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// Doc 08 P3 gate: taker fills reconcile against hand calculations.
    #[test]
    fn taker_fill_price_fee_rounding_and_impact_by_hand() {
        let mut v = venue(VenueConfig { impact_bps_per_million: 10.0, ..VenueConfig::default() });
        let mut lat = LatencyModel::new(LatencyConfig::zero());
        let m = bar(0, 2.99, 3.01, 3.0, 1.0);
        // Sell 1000.7 → 1000 contracts at 3.0 × (1 − 3.5 bp) = 2.99895;
        // fee = 1000 × 2.99895 × 3.5 bp + 0.03.
        let ev = v.execute(HedgeCommand::Submit(HedgeOrder { id: 1, size_units: -1000.7, spot: 3.0 }), OrderKind::Taker, 0, &m, &flat(), &mut lat);
        assert_eq!(ev[0].ev, VenueEvent::Hedge(HedgeEvent::Acknowledged(1)));
        let VenueEvent::Hedge(HedgeEvent::Filled(f)) = &ev[1].ev else { panic!("{:?}", ev[1]) };
        assert_eq!(f.size_units, -1000.0);
        assert!(near(f.price, 3.0 * (1.0 - 0.00035)));
        assert!(near(ev[1].fee, 1000.0 * f.price * 0.00035 + 0.03));
        assert!(near(ev[1].reference, 3.0));
        // Own impact: 10 bp per $1m → 2998.95 notional adds 0.02999 bp to the
        // next fill in the same direction (and the other: symmetric skew).
        let ev = v.execute(HedgeCommand::Submit(HedgeOrder { id: 2, size_units: 1.0, spot: 3.0 }), OrderKind::Taker, 0, &m, &flat(), &mut lat);
        let VenueEvent::Hedge(HedgeEvent::Filled(f2)) = &ev[1].ev else { panic!() };
        let impact = 10.0 * 1000.0 * f.price / 1_000_000.0;
        assert!(near(f2.price, 3.0 * (1.0 + (3.5 + impact) / 10_000.0)));
        // Below the minimum size: rejected.
        let ev = v.execute(HedgeCommand::Submit(HedgeOrder { id: 3, size_units: 0.4, spot: 3.0 }), OrderKind::Taker, 0, &m, &flat(), &mut lat);
        assert!(matches!(ev[0].ev, VenueEvent::Hedge(HedgeEvent::Rejected { order: 3, .. })));
        // Basis moves the mark, not the spot.
        let mut b = venue(VenueConfig { basis: vec![crate::scenario::BasisPoint { from_ms: 100, bps: 50.0 }], ..VenueConfig::default() });
        assert!(near(b.mark(&bar(0, 3.0, 3.0, 3.0, 1.0)), 3.0));
        assert!(near(b.mark(&bar(100, 3.0, 3.0, 3.0, 1.0)), 3.015));
        let ev = b.execute(HedgeCommand::Submit(HedgeOrder { id: 1, size_units: 10.0, spot: 3.0 }), OrderKind::Taker, 100, &bar(100, 3.0, 3.0, 3.0, 1.0), &flat(), &mut lat);
        let VenueEvent::Hedge(HedgeEvent::Filled(f)) = &ev[1].ev else { panic!() };
        assert!(near(f.price, 3.015 * (1.0 + 0.00035)));
    }

    /// Depth cap: a taker larger than the bar's depth fills across bars.
    #[test]
    fn taker_partial_fills_across_bars_when_depth_binds() {
        let mut v = venue(VenueConfig { max_taker_units_per_bar: 600.0, ..VenueConfig::default() });
        let mut lat = LatencyModel::new(LatencyConfig::zero());
        let m0 = bar(0, 3.0, 3.0, 3.0, 1.0);
        v.on_bar(&m0, &flat(), &mut lat);
        let ev = v.execute(HedgeCommand::Submit(HedgeOrder { id: 1, size_units: -1000.0, spot: 3.0 }), OrderKind::Taker, 0, &m0, &flat(), &mut lat);
        assert!(matches!(ev[1].ev, VenueEvent::Hedge(HedgeEvent::PartiallyFilled(Fill { size_units, .. })) if size_units == -600.0));
        // Same bar: depth consumed, nothing more.
        assert!(v.on_bar(&m0, &flat(), &mut lat).is_empty());
        let ev = v.on_bar(&bar(60_000, 3.1, 3.1, 3.1, 1.0), &flat(), &mut lat);
        assert!(matches!(ev[0].ev, VenueEvent::Hedge(HedgeEvent::Filled(Fill { order: 1, size_units, price })) if size_units == -400.0 && near(price, 3.1 * (1.0 - 0.00035))));
        assert!(v.resting.is_empty());
    }

    /// Doc 08 P3 gate: passive fixtures — partial fill, cancel race,
    /// no-fill — under each queue assumption.
    #[test]
    fn passive_partial_fill_cancel_race_and_no_fill() {
        let cfg = VenueConfig { execution_assumption: "central".into(), passive_participation: 0.5, queue_depth_units: 100.0, ..VenueConfig::default() };
        let mut v = venue(cfg.clone());
        let mut lat = LatencyModel::new(LatencyConfig::zero());
        let m = bar(0, 3.0, 3.0, 3.0, 1.0);
        // Buy 100 resting at 3.00 (central: 50 units of queue ahead).
        let ev = v.execute(HedgeCommand::Submit(HedgeOrder { id: 1, size_units: 100.0, spot: 3.0 }), OrderKind::Passive, 0, &m, &flat(), &mut lat);
        assert_eq!(ev.len(), 1, "passive: ack only");
        // A bar that never touches 3.00: no fill.
        assert!(v.on_bar(&bar(60_000, 3.01, 3.05, 3.02, 1000.0), &flat(), &mut lat).is_empty());
        // A bar with low 2.98, high 3.02, volume 1000: eligible = 1000 × (3.00 − 2.98)/0.04 = 500;
        // queue ahead 50 trades first; participation 0.5 × 450 = 225 → fills the 100 in full at 3.00, maker fee.
        let ev = v.on_bar(&bar(120_000, 2.98, 3.02, 3.0, 1000.0), &flat(), &mut lat);
        let VenueEvent::Hedge(HedgeEvent::Filled(f)) = &ev[0].ev else { panic!("{ev:?}") };
        assert!(near(f.price, 3.0) && f.size_units == 100.0);
        assert!(near(ev[0].fee, 100.0 * 3.0 * 0.0001 + 0.03));
        assert!(near(ev[0].reference, 3.0));
        // Partial: a thin bar fills part; the rest keeps resting.
        v.execute(HedgeCommand::Submit(HedgeOrder { id: 2, size_units: -100.0, spot: 3.0 }), OrderKind::Passive, 130_000, &m, &flat(), &mut lat);
        let ev = v.on_bar(&bar(180_000, 2.99, 3.01, 3.0, 140.0), &flat(), &mut lat);
        // eligible 70; queue 50 first; 0.5 × 20 = 10 → partial −10.
        assert!(matches!(ev[0].ev, VenueEvent::Hedge(HedgeEvent::PartiallyFilled(Fill { order: 2, size_units, .. })) if size_units == -10.0));
        assert!(near(v.resting[&2].remaining, -90.0));
        // Cancel race: the cancel takes 200 ms; a bar inside that window
        // still fills, the Cancelled event follows, and a later bar does not.
        let mut lat2 = LatencyModel::new(LatencyConfig { venue_cancel: LatencyDist::fixed(200), ..LatencyConfig::zero() });
        let ev = v.execute(HedgeCommand::Cancel(2), OrderKind::Passive, 240_000, &m, &flat(), &mut lat2);
        assert_eq!(ev[0], Timed::hedge(240_200, HedgeEvent::Cancelled(2)));
        let race = v.on_bar(&bar(240_100, 2.99, 3.01, 3.0, 1000.0), &flat(), &mut lat);
        assert!(matches!(race[0].ev, VenueEvent::Hedge(HedgeEvent::Filled(Fill { order: 2, size_units, .. })) if size_units == -90.0), "{race:?}");
        assert!(v.on_bar(&bar(300_000, 2.9, 3.1, 3.0, 1000.0), &flat(), &mut lat).is_empty());
        // Conservative needs a trade-through; optimistic fills on touch in full.
        let mut c = venue(VenueConfig { execution_assumption: "conservative".into(), through_bps: 10.0, ..cfg.clone() });
        c.execute(HedgeCommand::Submit(HedgeOrder { id: 1, size_units: 100.0, spot: 3.0 }), OrderKind::Passive, 0, &m, &flat(), &mut lat);
        assert!(c.on_bar(&bar(60_000, 2.999, 3.01, 3.0, 1000.0), &flat(), &mut lat).is_empty(), "touched but not through");
        // Through by 10 bp: eligible 500, queue 100 ahead, 0.5 × 400 = 200 ≥ 100 → filled.
        let ev = c.on_bar(&bar(120_000, 2.99, 3.01, 3.0, 1000.0), &flat(), &mut lat);
        assert!(matches!(ev[0].ev, VenueEvent::Hedge(HedgeEvent::Filled(_))), "{ev:?}");
        let mut o = venue(VenueConfig { execution_assumption: "optimistic".into(), ..cfg });
        o.execute(HedgeCommand::Submit(HedgeOrder { id: 1, size_units: 100.0, spot: 3.0 }), OrderKind::Passive, 0, &m, &flat(), &mut lat);
        let ev = o.on_bar(&bar(60_000, 3.0, 3.01, 3.0, 1.0), &flat(), &mut lat);
        assert!(matches!(ev[0].ev, VenueEvent::Hedge(HedgeEvent::Filled(Fill { size_units, .. })) if size_units == 100.0));
        assert_eq!(o.execution_assumption(), "optimistic");
    }

    /// The risk engine liquidates at the MARK (basis included), closes
    /// the position, forfeits the remaining margin, and cancels working
    /// orders; an outage rejects orders and top-ups but not liquidation.
    #[test]
    fn liquidation_uses_marks_and_outage_rejects_orders_and_topups() {
        let cfg = VenueConfig { basis: vec![crate::scenario::BasisPoint { from_ms: 0, bps: 100.0 }], ..VenueConfig::default() };
        let mut v = SimPerpVenue::new(cfg, MarginConfig { outages: vec![[500_000, 600_000]], ..MarginConfig::default() }, 3.5, 3.5, 0.03);
        let mut lat = LatencyModel::new(LatencyConfig::zero());
        // Short 1000 @ 3.00 with 300 margin: P_liq = 3300/1025 = 3.2195.
        let acct = IsolatedPosition { size: -1000.0, entry: 3.0, margin: 300.0 };
        // Spot 3.20 is below the liquidation price, but the mark is
        // 3.20 × 1.01 = 3.232: liquidated at the mark.
        v.execute(HedgeCommand::Submit(HedgeOrder { id: 9, size_units: 1.0, spot: 3.0 }), OrderKind::Taker, 0, &bar(0, 3.0, 3.0, 3.0, 1.0), &acct, &mut lat);
        assert!(v.on_bar(&bar(60_000, 3.18, 3.18, 3.18, 1.0), &acct, &mut lat).is_empty(), "mark 3.2118 < P_liq");
        let ev = v.on_bar(&bar(120_000, 3.2, 3.2, 3.2, 1.0), &acct, &mut lat);
        let Some(Timed { ev: VenueEvent::Liquidated { size_closed, price, penalty, full }, .. }) = ev.last() else { panic!("{ev:?}") };
        assert!(*full && near(*size_closed, 1000.0) && near(*price, 3.232));
        // Remaining account value = 300 + (−1000 × 0.232) = 68 → forfeited.
        assert!(near(*penalty, 300.0 - 232.0));
        assert_eq!(v.liquidations, 1);
        assert!(v.resting.is_empty());
        // Outage: orders rejected, top-up refused, but a liquidation still runs.
        let ev = v.execute(HedgeCommand::Submit(HedgeOrder { id: 10, size_units: 10.0, spot: 3.0 }), OrderKind::Taker, 550_000, &bar(550_000, 3.0, 3.0, 3.0, 1.0), &flat(), &mut lat);
        assert!(matches!(&ev[0].ev, VenueEvent::Hedge(HedgeEvent::Rejected { reason, .. }) if reason == "venue outage"));
        assert_eq!(v.topup(50.0, 550_000)[0].ev, VenueEvent::TopUp { amount: 50.0, accepted: false });
        assert_eq!(v.topup(50.0, 600_000)[0].ev, VenueEvent::TopUp { amount: 50.0, accepted: true });
        let ev = v.on_bar(&bar(560_000, 3.3, 3.3, 3.3, 1.0), &acct, &mut lat);
        assert!(matches!(ev.last().map(|t| &t.ev), Some(VenueEvent::Liquidated { .. })));
        // Partial liquidation (assumption): closes the fraction with a penalty.
        let mut p = SimPerpVenue::new(VenueConfig::default(), MarginConfig { partial_close: 0.5, partial_penalty_bps: 100.0, ..MarginConfig::default() }, 3.5, 3.5, 0.03);
        let ev = p.on_bar(&bar(0, 3.3, 3.3, 3.3, 1.0), &acct, &mut lat);
        let Some(Timed { ev: VenueEvent::Liquidated { size_closed, penalty, full, .. }, .. }) = ev.last() else { panic!() };
        assert!(!*full && near(*size_closed, 500.0) && near(*penalty, 500.0 * 3.3 * 0.01));
    }

    #[test]
    fn open_orders_track_partials_cancels_and_late_fills() {
        let mut open = OpenOrders::default();
        open.submit(&HedgeOrder { id: 1, size_units: -100.0, spot: 10.0 }, 0);
        assert_eq!(plan_hedge_order(100.0, 0.0, open.working_units(), 10.0), None);
        let (f, spot) = open.apply(&HedgeEvent::PartiallyFilled(Fill { order: 1, size_units: -40.0, price: 10.0 })).unwrap();
        assert_eq!((f.size_units, spot), (-40.0, Some(10.0)));
        assert_eq!(open.working_units(), -60.0);
        open.apply(&HedgeEvent::PartiallyFilled(Fill { order: 1, size_units: -60.0, price: 10.0 }));
        assert!(open.is_empty());
        open.submit(&HedgeOrder { id: 7, size_units: 50.0, spot: 10.0 }, 0);
        assert_eq!(open.stale(60_000, 60_000), vec![7]);
        assert!(open.apply(&HedgeEvent::Cancelled(7)).is_none());
        let late = open.apply(&HedgeEvent::Filled(Fill { order: 7, size_units: 50.0, price: 10.0 })).unwrap();
        assert_eq!((late.0.size_units, late.1), (50.0, None));
        assert_eq!(rebalance_target(-80.0, 0.0, 50.0), Some(80.0));
        assert_eq!(rebalance_target(-80.0, 80.0, 50.0), None);
    }
}
