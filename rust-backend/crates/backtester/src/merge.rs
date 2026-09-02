//! Bounded-memory k-way merge of external event sources (doc 08 §6.5).
//! Each source is a pull iterator that holds at most one Arrow batch (or
//! one in-memory slice) at a time; the merge holds one head per source
//! and yields rows in `(ts, source index)` order, so ties between
//! sources always replay in roster order. The 200 ms reduced series is
//! the floor: a source may be wrapped so that only the last row per slot
//! survives (exact for every sampling grid the estimators use); the
//! sub-slot raw rows are for execution only.

use std::collections::VecDeque;

use crate::data::{Bar, FundingRow};

/// Slot width of the reduced series (data-room `gold::read::REDUCE_SLOT_MS`).
pub const REDUCE_SLOT_MS: i64 = 200;

/// One external row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum External {
    Bar(Bar),
    Funding(FundingRow),
    VolIndex { ts_ms: i64, pct: f64 },
}

impl External {
    pub fn ts_ms(&self) -> i64 {
        match self {
            External::Bar(b) => b.ts_ms,
            External::Funding(f) => f.ts_ms,
            External::VolIndex { ts_ms, .. } => *ts_ms,
        }
    }

    /// The feed name used by coverage tracking.
    pub fn feed(&self) -> &'static str {
        match self {
            External::Bar(_) => "spot",
            External::Funding(_) => "funding",
            External::VolIndex { .. } => "vol_index",
        }
    }
}

/// A pull source of ascending external rows.
pub trait EventSource {
    fn name(&self) -> &str;
    fn next_row(&mut self) -> anyhow::Result<Option<External>>;
    /// Rows yielded so far.
    fn rows(&self) -> u64;
}

/// An in-memory slice as a source (tests, sweeps).
pub struct SliceSource {
    name: String,
    rows: VecDeque<External>,
    yielded: u64,
}

impl SliceSource {
    pub fn new(name: &str, rows: Vec<External>) -> Self {
        Self { name: name.into(), rows: rows.into(), yielded: 0 }
    }

    pub fn bars(bars: &[Bar]) -> Self {
        Self::new("spot", bars.iter().map(|b| External::Bar(*b)).collect())
    }

    pub fn funding(rows: &[FundingRow]) -> Self {
        Self::new("funding", rows.iter().map(|r| External::Funding(*r)).collect())
    }

    pub fn vol_index(rows: &[(i64, f64)]) -> Self {
        Self::new("vol_index", rows.iter().map(|(t, p)| External::VolIndex { ts_ms: *t, pct: *p }).collect())
    }
}

impl EventSource for SliceSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn next_row(&mut self) -> anyhow::Result<Option<External>> {
        let r = self.rows.pop_front();
        if r.is_some() {
            self.yielded += 1;
        }
        Ok(r)
    }

    fn rows(&self) -> u64 {
        self.yielded
    }
}

/// Keeps the last row per `slot_ms` slot of the wrapped source.
pub struct Reduced<S> {
    inner: S,
    slot_ms: i64,
    pending: Option<External>,
    done: bool,
}

impl<S: EventSource> Reduced<S> {
    pub fn new(inner: S, slot_ms: i64) -> Self {
        Self { inner, slot_ms: slot_ms.max(1), pending: None, done: false }
    }
}

impl<S: EventSource> EventSource for Reduced<S> {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn next_row(&mut self) -> anyhow::Result<Option<External>> {
        loop {
            if self.done {
                return Ok(self.pending.take());
            }
            match self.inner.next_row()? {
                None => {
                    self.done = true;
                }
                Some(row) => match self.pending {
                    Some(p) if p.ts_ms().div_euclid(self.slot_ms) == row.ts_ms().div_euclid(self.slot_ms) => {
                        self.pending = Some(row);
                    }
                    Some(p) => {
                        self.pending = Some(row);
                        return Ok(Some(p));
                    }
                    None => self.pending = Some(row),
                },
            }
        }
    }

    fn rows(&self) -> u64 {
        self.inner.rows()
    }
}

/// The merge: one head per source, popped in `(ts, source index)` order.
pub struct Merge {
    sources: Vec<Box<dyn EventSource>>,
    heads: Vec<Option<External>>,
    exhausted: usize,
}

impl Merge {
    pub fn new(sources: Vec<Box<dyn EventSource>>) -> anyhow::Result<Self> {
        let mut heads = Vec::with_capacity(sources.len());
        let mut m = Self { sources, heads: Vec::new(), exhausted: 0 };
        for i in 0..m.sources.len() {
            let h = m.sources[i].next_row()?;
            if h.is_none() {
                m.exhausted += 1;
            }
            heads.push(h);
        }
        m.heads = heads;
        Ok(m)
    }

    fn min_index(&self) -> Option<usize> {
        let mut best: Option<(i64, usize)> = None;
        for (i, h) in self.heads.iter().enumerate() {
            if let Some(r) = h {
                let key = (r.ts_ms(), i);
                if best.is_none_or(|b| key < b) {
                    best = Some(key);
                }
            }
        }
        best.map(|(_, i)| i)
    }

    /// Timestamp of the next row without consuming it.
    pub fn peek_ts(&self) -> Option<i64> {
        self.min_index().and_then(|i| self.heads[i].map(|r| r.ts_ms()))
    }

    pub fn next_row(&mut self) -> anyhow::Result<Option<External>> {
        let Some(i) = self.min_index() else { return Ok(None) };
        let row = self.heads[i].take();
        self.heads[i] = self.sources[i].next_row()?;
        if self.heads[i].is_none() {
            self.exhausted += 1;
        }
        Ok(row)
    }

    /// Rows yielded per source, in roster order.
    pub fn row_counts(&self) -> Vec<(String, u64)> {
        self.sources.iter().map(|s| (s.name().to_string(), s.rows())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(ts: i64) -> Bar {
        Bar { ts_ms: ts, open: 1.0, high: 1.0, low: 1.0, close: 1.0, volume: 1.0 }
    }

    #[test]
    fn merge_orders_by_ts_then_roster_and_counts_rows() {
        let bars = SliceSource::bars(&[bar(0), bar(60_000), bar(120_000)]);
        let funding = SliceSource::funding(&[FundingRow { ts_ms: 60_000, rate: 0.0, interval_hours: 8.0 }]);
        let idx = SliceSource::vol_index(&[(60_000, 50.0), (90_000, 51.0)]);
        let mut m = Merge::new(vec![Box::new(bars), Box::new(funding), Box::new(idx)]).unwrap();
        let mut order = Vec::new();
        while let Some(r) = m.next_row().unwrap() {
            order.push((r.ts_ms(), r.feed()));
        }
        assert_eq!(order, [
            (0, "spot"), (60_000, "spot"), (60_000, "funding"), (60_000, "vol_index"), (90_000, "vol_index"), (120_000, "spot"),
        ]);
        assert_eq!(m.row_counts(), vec![("spot".to_string(), 3), ("funding".to_string(), 1), ("vol_index".to_string(), 2)]);
        assert!(m.peek_ts().is_none());
    }

    #[test]
    fn reduced_keeps_the_last_row_per_slot() {
        let raw = SliceSource::bars(&[bar(0), bar(50), bar(199), bar(200), bar(450), bar(460)]);
        let mut r = Reduced::new(raw, REDUCE_SLOT_MS);
        let mut got = Vec::new();
        while let Some(x) = r.next_row().unwrap() {
            got.push(x.ts_ms());
        }
        assert_eq!(got, [199, 200, 460]);
        assert_eq!(r.rows(), 6, "raw rows are still counted for reconciliation");
    }
}
