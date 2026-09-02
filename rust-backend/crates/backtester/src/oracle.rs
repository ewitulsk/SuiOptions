//! The oracle proxy (doc 08 §6.1 restated oracle-neutral in doc 09 §3):
//! bar closes become decision-price observations on a fixed cadence,
//! actionable only after the configured latency, and stale after
//! `max_age_ms`. The strategy never sees the market faster or cleaner
//! than the live oracle path would deliver it.

use crate::scenario::OracleModel;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Observation {
    /// Event time of the underlying bar.
    pub event_ms: i64,
    /// When the strategy could first act on it.
    pub actionable_ms: i64,
    pub price: f64,
    pub conf: f64,
}

#[derive(Debug)]
pub struct OracleProxy {
    model: OracleModel,
    last_published_ms: i64,
    latest: Option<Observation>,
    /// The observation before `latest`: what the strategy still acts on
    /// while the newest one is in flight.
    previous: Option<Observation>,
}

impl OracleProxy {
    pub fn new(model: OracleModel) -> Self {
        Self { model, last_published_ms: i64::MIN, latest: None, previous: None }
    }

    /// Feed one bar close; publishes only on the model's cadence. The
    /// observation becomes actionable after the model's latency plus
    /// `extra_latency_ms` (the per-stage observation draw, doc 08 §6.3).
    /// Returns the published observation so the engine can schedule it.
    pub fn observe(&mut self, event_ms: i64, close: f64, extra_latency_ms: i64) -> Option<Observation> {
        if event_ms.saturating_sub(self.last_published_ms) < self.model.update_ms {
            return None;
        }
        self.last_published_ms = event_ms;
        self.previous = self.latest;
        let obs = Observation {
            event_ms,
            actionable_ms: event_ms + self.model.latency_ms + extra_latency_ms.max(0),
            price: close,
            conf: close * self.model.conf_bps / 10_000.0,
        };
        self.latest = Some(obs);
        Some(obs)
    }

    /// The decision price at `now_ms`: the latest observation that is
    /// already actionable and not stale. `None` = the desk declines.
    pub fn decision(&self, now_ms: i64) -> Option<Observation> {
        let fresh = |o: &Observation| now_ms >= o.actionable_ms && now_ms - o.event_ms <= self.model.max_age_ms;
        match (self.latest, self.previous) {
            (Some(l), _) if fresh(&l) => Some(l),
            (_, Some(p)) if fresh(&p) => Some(p),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_and_staleness_gate_the_decision_price() {
        let mut o = OracleProxy::new(OracleModel { update_ms: 60_000, latency_ms: 2_000, conf_bps: 0.0, max_age_ms: 180_000 });
        o.observe(0, 100.0, 0);
        assert!(o.decision(1_000).is_none(), "not actionable before latency");
        assert_eq!(o.decision(2_000).map(|d| d.price), Some(100.0));
        // Sub-cadence updates are ignored (the oracle would not publish).
        o.observe(30_000, 101.0, 0);
        assert_eq!(o.decision(32_000).map(|d| d.price), Some(100.0));
        // A new publish in flight: the previous one stays actionable.
        o.observe(60_000, 103.0, 0);
        assert_eq!(o.decision(60_000).map(|d| d.price), Some(100.0));
        assert_eq!(o.decision(62_000).map(|d| d.price), Some(103.0));
        // Stale after max_age: a capture hole never yields a fresh price.
        assert!(o.decision(240_001).is_none());
        o.observe(300_000, 102.0, 0);
        assert_eq!(o.decision(302_000).map(|d| d.price), Some(102.0));
    }
}
