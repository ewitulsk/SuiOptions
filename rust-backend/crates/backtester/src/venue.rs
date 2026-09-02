//! The simulated hedge-venue seam. The types mirror
//! `services/mm-bot/src/desk/hedge.rs` (`HedgeOrder`, `HedgeCommand`,
//! `Fill`, `HedgeEvent`, `OpenOrders`) one to one — commands in, events
//! out, signed sizes, fill-after-cancel resolved in the venue's favour —
//! so the engine drives a venue exactly the way the desk's rebalancer
//! drives a live one. They are mirrored rather than imported because
//! `mm-bot` links the Sui SDK; a shared `desk-core` (doc 08 §5.2) is
//! where both will come from.
//!
//! The one venue here is the v0 taker: every order fills in full at the
//! desk's reference price ± slippage the moment it reaches the venue.
//! The full lifecycle (passive placement, partial fills, cancel races,
//! own impact, margin) is doc 08 PR L.

use std::collections::BTreeMap;

use crate::latency::{LatencyModel, LatencyStage};

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

/// An event stamped with the instant the DESK learns of it (the venue's
/// own execution time plus reporting latency).
#[derive(Clone, Debug, PartialEq)]
pub struct Timed {
    pub at_ms: i64,
    pub ev: HedgeEvent,
}

/// Market truth as the venue sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MarketState {
    pub ts_ms: i64,
    /// Last bar close (never the decision price).
    pub spot: f64,
}

/// A simulated perp venue: commands arrive after submit latency; the
/// venue answers with events stamped for the desk's clock.
pub trait SimVenue {
    fn name(&self) -> &str;
    /// The queue/fill assumption every output is labeled with.
    fn execution_assumption(&self) -> &'static str;
    /// One command, arriving at the venue at `arrival_ms`.
    fn execute(&mut self, cmd: HedgeCommand, arrival_ms: i64, market: &MarketState, lat: &mut LatencyModel) -> Vec<Timed>;
    /// A new bar of market truth (resting orders, marks).
    fn on_bar(&mut self, _market: &MarketState, _lat: &mut LatencyModel) -> Vec<Timed> {
        Vec::new()
    }
}

/// v0 taker venue: full fill at `order.spot ± slippage` on arrival.
#[derive(Clone, Debug)]
pub struct TakerVenue {
    pub slippage_bps: f64,
}

impl SimVenue for TakerVenue {
    fn name(&self) -> &str {
        "taker"
    }

    fn execution_assumption(&self) -> &'static str {
        "taker_only"
    }

    fn execute(&mut self, cmd: HedgeCommand, arrival_ms: i64, _market: &MarketState, lat: &mut LatencyModel) -> Vec<Timed> {
        match cmd {
            HedgeCommand::Submit(order) => {
                if order.size_units == 0.0 || order.spot <= 0.0 {
                    let at = arrival_ms + lat.draw(LatencyStage::VenueAck);
                    return vec![Timed { at_ms: at, ev: HedgeEvent::Rejected { order: order.id, reason: "zero size or non-positive spot".into() } }];
                }
                let ack_ms = arrival_ms + lat.draw(LatencyStage::VenueAck);
                let fill_ms = arrival_ms + lat.draw(LatencyStage::VenueFillReport);
                let slip = order.spot * self.slippage_bps / 10_000.0;
                let price = order.spot + slip * order.size_units.signum();
                vec![
                    Timed { at_ms: ack_ms, ev: HedgeEvent::Acknowledged(order.id) },
                    Timed { at_ms: fill_ms, ev: HedgeEvent::Filled(Fill { order: order.id, size_units: order.size_units, price }) },
                ]
            }
            HedgeCommand::Cancel(id) => {
                let at = arrival_ms + lat.draw(LatencyStage::VenueCancel);
                vec![Timed { at_ms: at, ev: HedgeEvent::Cancelled(id) }]
            }
            HedgeCommand::Replace { old, new } => {
                let mut out = self.execute(HedgeCommand::Cancel(old), arrival_ms, _market, lat);
                out.extend(self.execute(HedgeCommand::Submit(new), arrival_ms, _market, lat));
                out
            }
        }
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
    use crate::latency::LatencyConfig;

    #[test]
    fn taker_fills_in_full_at_reference_plus_slip_with_latencies() {
        let mut v = TakerVenue { slippage_bps: 10.0 };
        let mut lat = LatencyModel::new(LatencyConfig::zero());
        let m = MarketState { ts_ms: 0, spot: 99.0 };
        let ev = v.execute(HedgeCommand::Submit(HedgeOrder { id: 1, size_units: -10.0, spot: 100.0 }), 5, &m, &mut lat);
        assert_eq!(ev[0], Timed { at_ms: 5, ev: HedgeEvent::Acknowledged(1) });
        assert_eq!(ev[1], Timed { at_ms: 5, ev: HedgeEvent::Filled(Fill { order: 1, size_units: -10.0, price: 99.9 }) });
        let mut lat = LatencyModel::new(LatencyConfig { venue_ack: crate::latency::LatencyDist::fixed(7), venue_fill_report: crate::latency::LatencyDist::fixed(30), ..LatencyConfig::zero() });
        let ev = v.execute(HedgeCommand::Submit(HedgeOrder { id: 2, size_units: 4.0, spot: 100.0 }), 100, &m, &mut lat);
        assert_eq!(ev[0].at_ms, 107);
        assert_eq!(ev[1].at_ms, 130);
        assert!(matches!(ev[1].ev, HedgeEvent::Filled(Fill { price, .. }) if (price - 100.1).abs() < 1e-12));
        let ev = v.execute(HedgeCommand::Submit(HedgeOrder { id: 3, size_units: 0.0, spot: 100.0 }), 0, &m, &mut lat);
        assert!(matches!(ev[0].ev, HedgeEvent::Rejected { order: 3, .. }));
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
