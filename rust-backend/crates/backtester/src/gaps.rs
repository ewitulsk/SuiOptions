//! Gap policy (doc 08 §6.4): gaps are uncertainty, not frozen time. A
//! run declares its required feeds; when one goes quiet for longer than
//! its declared cadence allows, the span is a *gap*. Nothing stops in a
//! gap — timers, TTLs, funding, expiry, margin and pending transactions
//! keep running against ageing cached data, and the production staleness
//! gates fire on their own — but any outcome that would have needed the
//! missing truth (a settlement, a fill, a mark) is either bounded
//! conservatively or the span is invalidated, and the run says which.
//! Coverage, gaps and invalidated spans are part of every output.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `[gaps]` in the scenario.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct GapConfig {
    /// Feeds the run cannot do without (`spot`, `funding`, `vol_index`).
    pub required_feeds: Vec<String>,
    /// A required feed silent for longer than this is in a gap.
    pub max_gap_ms: i64,
    /// `invalidate`: every span in which an outcome needed the missing
    /// truth is reported as invalidated. `bound`: the outcome is applied
    /// at the conservative bound (last known price) and reported as
    /// bounded. Either way the run never earns from a hole.
    pub policy: String,
}

impl Default for GapConfig {
    fn default() -> Self {
        Self { required_feeds: vec!["spot".into()], max_gap_ms: 5 * 60_000, policy: "invalidate".into() }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct GapSpan {
    pub feed: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct InvalidatedSpan {
    pub feed: String,
    pub start_ms: i64,
    pub end_ms: i64,
    /// What needed the missing truth.
    pub reason: String,
    /// True when the outcome was applied at a conservative bound rather
    /// than the span being thrown out (`policy = "bound"`).
    pub bounded: bool,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct FeedCoverage {
    pub rows: u64,
    pub first_ms: Option<i64>,
    pub last_ms: Option<i64>,
    /// Milliseconds of the run window NOT inside a gap.
    pub covered_ms: i64,
    pub fraction: f64,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Coverage {
    pub policy: String,
    pub required_feeds: Vec<String>,
    pub feeds: BTreeMap<String, FeedCoverage>,
    pub gaps: Vec<GapSpan>,
    pub invalidated_spans: Vec<InvalidatedSpan>,
}

#[derive(Debug)]
struct FeedState {
    required: bool,
    last_ms: Option<i64>,
    rows: u64,
    first_ms: Option<i64>,
    /// Open gap start, if the feed is currently silent past the limit.
    open_gap: Option<i64>,
}

/// Tracks feed liveness across the run window `[start_ms, end_ms)`.
#[derive(Debug)]
pub struct GapTracker {
    cfg: GapConfig,
    start_ms: i64,
    end_ms: i64,
    feeds: BTreeMap<String, FeedState>,
    gaps: Vec<GapSpan>,
    invalidated: Vec<InvalidatedSpan>,
}

impl GapTracker {
    pub fn new(cfg: GapConfig, start_ms: i64, end_ms: i64) -> Self {
        let mut feeds = BTreeMap::new();
        for f in &cfg.required_feeds {
            feeds.insert(f.clone(), FeedState { required: true, last_ms: None, rows: 0, first_ms: None, open_gap: None });
        }
        Self { cfg, start_ms, end_ms, feeds, gaps: Vec::new(), invalidated: Vec::new() }
    }

    fn feed(&mut self, name: &str) -> &mut FeedState {
        self.feeds
            .entry(name.to_string())
            .or_insert(FeedState { required: false, last_ms: None, rows: 0, first_ms: None, open_gap: None })
    }

    /// One row of `feed` at `ts_ms`. Closes an open gap.
    pub fn observe(&mut self, feed: &str, ts_ms: i64) {
        let max_gap = self.cfg.max_gap_ms;
        let (start, end) = (self.start_ms, self.end_ms);
        let st = self.feed(feed);
        st.rows += 1;
        if st.first_ms.is_none() {
            st.first_ms = Some(ts_ms);
        }
        let gap = match (st.required, st.open_gap, st.last_ms) {
            (true, Some(g), _) => Some(g),
            (true, None, Some(last)) if ts_ms - last > max_gap => Some(last + max_gap),
            (true, None, None) if ts_ms - start > max_gap => Some(start + max_gap),
            _ => None,
        };
        st.last_ms = Some(ts_ms);
        st.open_gap = None;
        if let Some(g) = gap {
            let (a, b) = (g.max(start), ts_ms.min(end));
            if b > a {
                self.gaps.push(GapSpan { feed: feed.to_string(), start_ms: a, end_ms: b });
            }
        }
    }

    /// Advance the clock: a required feed silent past the limit opens a
    /// gap at `last + max_gap` (or the run start when it never spoke).
    pub fn tick(&mut self, now_ms: i64) {
        let max_gap = self.cfg.max_gap_ms;
        let start = self.start_ms;
        for st in self.feeds.values_mut() {
            if !st.required || st.open_gap.is_some() {
                continue;
            }
            let since = st.last_ms.unwrap_or(start);
            if now_ms - since > max_gap {
                st.open_gap = Some(since.max(start - max_gap) + max_gap);
            }
        }
    }

    /// Whether `feed` is inside a gap at `now_ms` (after `tick`).
    pub fn in_gap(&self, feed: &str, now_ms: i64) -> bool {
        match self.feeds.get(feed) {
            Some(st) if st.required => match (st.open_gap, st.last_ms) {
                (Some(g), _) => now_ms >= g,
                (None, Some(last)) => now_ms - last > self.cfg.max_gap_ms,
                (None, None) => now_ms - self.start_ms > self.cfg.max_gap_ms,
            },
            _ => false,
        }
    }

    /// Any required feed in a gap now.
    pub fn any_gap(&self, now_ms: i64) -> bool {
        self.feeds.iter().any(|(f, st)| st.required && self.in_gap(f, now_ms))
    }

    /// Record that an outcome at `now_ms` needed the truth of `feed`
    /// while it was in a gap. Returns whether the outcome should be
    /// applied at a conservative bound (`bound`) or the span invalidated.
    pub fn needed_truth(&mut self, feed: &str, now_ms: i64, reason: &str) -> bool {
        let bounded = self.cfg.policy == "bound";
        let start = self.feeds.get(feed).and_then(|st| st.open_gap).unwrap_or(now_ms);
        self.invalidated.push(InvalidatedSpan {
            feed: feed.to_string(),
            start_ms: start.max(self.start_ms),
            end_ms: now_ms,
            reason: reason.to_string(),
            bounded,
        });
        bounded
    }

    /// Close open gaps at the end of the run and summarize.
    pub fn finish(mut self) -> Coverage {
        let end = self.end_ms;
        let names: Vec<String> = self.feeds.keys().cloned().collect();
        for name in &names {
            let st = self.feeds.get_mut(name).expect("listed");
            if let Some(g) = st.open_gap.take() {
                if end > g {
                    self.gaps.push(GapSpan { feed: name.clone(), start_ms: g.max(self.start_ms), end_ms: end });
                }
            }
        }
        // Extend invalidated spans whose gap was still open to the gap end.
        for inv in &mut self.invalidated {
            if let Some(g) = self.gaps.iter().find(|g| g.feed == inv.feed && g.start_ms <= inv.start_ms && g.end_ms >= inv.end_ms) {
                inv.end_ms = g.end_ms;
            }
        }
        self.gaps.sort_by_key(|g| (g.start_ms, g.feed.clone()));
        let window = (end - self.start_ms).max(1);
        let mut feeds = BTreeMap::new();
        for (name, st) in &self.feeds {
            let gap_ms: i64 = self.gaps.iter().filter(|g| &g.feed == name).map(|g| g.end_ms - g.start_ms).sum();
            let covered = (window - gap_ms).max(0);
            feeds.insert(
                name.clone(),
                FeedCoverage {
                    rows: st.rows,
                    first_ms: st.first_ms,
                    last_ms: st.last_ms,
                    covered_ms: covered,
                    fraction: covered as f64 / window as f64,
                },
            );
        }
        Coverage {
            policy: self.cfg.policy.clone(),
            required_feeds: self.cfg.required_feeds.clone(),
            feeds,
            gaps: self.gaps,
            invalidated_spans: self.invalidated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gaps_open_on_silence_close_on_the_next_row_and_invalidate_outcomes() {
        let cfg = GapConfig { required_feeds: vec!["spot".into()], max_gap_ms: 100, policy: "invalidate".into() };
        let mut g = GapTracker::new(cfg, 0, 1_000);
        for t in (0..=300).step_by(50) {
            g.observe("spot", t);
            g.tick(t);
            assert!(!g.in_gap("spot", t));
        }
        // Silence from 300: the gap opens at 400.
        g.tick(350);
        assert!(!g.in_gap("spot", 350));
        g.tick(450);
        assert!(g.in_gap("spot", 450));
        assert!(!g.needed_truth("spot", 500, "expiry settlement"));
        g.observe("spot", 600);
        assert!(!g.in_gap("spot", 600));
        // Optional feeds never gap.
        g.observe("funding", 0);
        g.tick(900);
        assert!(!g.in_gap("funding", 900));
        // Silence to the end: a trailing gap from 700.
        let cov = g.finish();
        assert_eq!(cov.gaps, vec![
            GapSpan { feed: "spot".into(), start_ms: 400, end_ms: 600 },
            GapSpan { feed: "spot".into(), start_ms: 700, end_ms: 1_000 },
        ]);
        assert_eq!(cov.invalidated_spans.len(), 1);
        let inv = &cov.invalidated_spans[0];
        assert_eq!((inv.start_ms, inv.end_ms, inv.bounded), (400, 600, false));
        let spot = &cov.feeds["spot"];
        assert_eq!(spot.rows, 8);
        assert_eq!(spot.covered_ms, 500);
        assert!((spot.fraction - 0.5).abs() < 1e-12);
        assert_eq!(cov.feeds["funding"].fraction, 1.0);
    }

    #[test]
    fn bound_policy_marks_outcomes_bounded_and_a_mute_feed_gaps_from_start() {
        let cfg = GapConfig { required_feeds: vec!["spot".into()], max_gap_ms: 10, policy: "bound".into() };
        let mut g = GapTracker::new(cfg, 100, 200);
        g.tick(120);
        assert!(g.in_gap("spot", 120));
        assert!(g.needed_truth("spot", 130, "fill"));
        let cov = g.finish();
        assert_eq!(cov.gaps, vec![GapSpan { feed: "spot".into(), start_ms: 110, end_ms: 200 }]);
        assert!(cov.invalidated_spans[0].bounded);
        assert_eq!(cov.invalidated_spans[0].end_ms, 200);
    }
}
