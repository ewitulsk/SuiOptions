//! Bar intervals + carry-forward gap filling (pure; unit-tested).
//!
//! Bars come from real fills only. Empty buckets inside the requested range
//! are filled with flat candles at the previous close and volume 0 — the
//! standard exchange-chart convention (decided on the epic). Leading buckets
//! before the first trade ever are omitted, not invented.

use serde::Serialize;

/// Supported chart intervals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Interval {
    M1,
    M5,
    M15,
    H1,
    H4,
    D1,
}

impl Interval {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "1m" => Some(Self::M1),
            "5m" => Some(Self::M5),
            "15m" => Some(Self::M15),
            "1h" => Some(Self::H1),
            "4h" => Some(Self::H4),
            "1d" => Some(Self::D1),
            _ => None,
        }
    }

    pub fn secs(self) -> i64 {
        match self {
            Self::M1 => 60,
            Self::M5 => 300,
            Self::M15 => 900,
            Self::H1 => 3_600,
            Self::H4 => 14_400,
            Self::D1 => 86_400,
        }
    }

    pub fn ms(self) -> i64 {
        self.secs() * 1_000
    }
}

/// One served bar. `t` = bucket start (unix ms); `v` = base volume in
/// display units. Field names map 1:1 onto Lightweight Charts' candlestick
/// data points.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct Bar {
    pub t: i64,
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
    pub v: f64,
}

/// A trade-bearing bucket from SQL, already aligned to the interval grid.
#[derive(Debug, Clone, Copy)]
pub struct TradedBar {
    pub t: i64,
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
    pub v: f64,
}

/// Fill `[from_ms, to_ms)` on the interval grid. `seed_close` is the last
/// trade price before `from_ms` (carries the line through a leading gap);
/// with no seed and no earlier trade, leading empty buckets are skipped.
pub fn carry_forward(
    traded: &[TradedBar],
    interval: Interval,
    from_ms: i64,
    to_ms: i64,
    seed_close: Option<f64>,
) -> Vec<Bar> {
    let step = interval.ms();
    let first_bucket = align_down(from_ms, step);
    let mut out = Vec::new();
    let mut prev_close = seed_close;
    let mut idx = 0usize;

    let mut t = first_bucket;
    while t < to_ms {
        if idx < traded.len() && traded[idx].t == t {
            let b = traded[idx];
            out.push(Bar { t, o: b.o, h: b.h, l: b.l, c: b.c, v: b.v });
            prev_close = Some(b.c);
            idx += 1;
        } else if let Some(c) = prev_close {
            out.push(Bar { t, o: c, h: c, l: c, c, v: 0.0 });
        }
        // else: before the first trade ever — emit nothing.
        t += step;
    }
    out
}

pub fn align_down(ms: i64, step_ms: i64) -> i64 {
    (ms / step_ms) * step_ms
}

/// One served midpoint. `t` = bucket start (unix ms); `m` = mid in display
/// units (quote per base). Maps onto a Lightweight Charts line-series point.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct MidPoint {
    pub t: i64,
    pub m: f64,
}

/// A sample-bearing midpoint bucket from SQL, already aligned to the grid.
#[derive(Debug, Clone, Copy)]
pub struct SampledMid {
    pub t: i64,
    pub m: f64,
}

/// Fill `[from_ms, to_ms)` on the interval grid — the mid-line counterpart
/// of [`carry_forward`]: empty buckets carry the previous mid, leading
/// buckets before the first sample ever are omitted unless seeded.
pub fn carry_forward_mids(
    sampled: &[SampledMid],
    interval: Interval,
    from_ms: i64,
    to_ms: i64,
    seed_mid: Option<f64>,
) -> Vec<MidPoint> {
    let step = interval.ms();
    let mut out = Vec::new();
    let mut prev = seed_mid;
    let mut idx = 0usize;

    let mut t = align_down(from_ms, step);
    while t < to_ms {
        if idx < sampled.len() && sampled[idx].t == t {
            let m = sampled[idx].m;
            out.push(MidPoint { t, m });
            prev = Some(m);
            idx += 1;
        } else if let Some(m) = prev {
            out.push(MidPoint { t, m });
        }
        // else: before the first sample ever — emit nothing.
        t += step;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tb(t: i64, px: f64, v: f64) -> TradedBar {
        TradedBar { t, o: px, h: px + 1.0, l: px - 1.0, c: px, v }
    }

    #[test]
    fn interval_parsing() {
        assert_eq!(Interval::parse("1m"), Some(Interval::M1));
        assert_eq!(Interval::parse("4h"), Some(Interval::H4));
        assert_eq!(Interval::parse("2h"), None);
        assert_eq!(Interval::D1.ms(), 86_400_000);
    }

    #[test]
    fn gaps_fill_flat_with_zero_volume() {
        let step = Interval::M1.ms();
        let traded = [tb(0, 10.0, 1.0), tb(3 * step, 12.0, 2.0)];
        let bars = carry_forward(&traded, Interval::M1, 0, 5 * step, None);
        assert_eq!(bars.len(), 5);
        // Buckets 1 and 2 are flat at bucket 0's close.
        assert_eq!(bars[1], Bar { t: step, o: 10.0, h: 10.0, l: 10.0, c: 10.0, v: 0.0 });
        assert_eq!(bars[2].c, 10.0);
        assert_eq!(bars[3].c, 12.0);
        // Trailing gap carries the new close.
        assert_eq!(bars[4].o, 12.0);
        assert_eq!(bars[4].v, 0.0);
    }

    #[test]
    fn leading_gap_omitted_without_seed_but_filled_with_one() {
        let step = Interval::M5.ms();
        let traded = [tb(2 * step, 5.0, 1.0)];

        let no_seed = carry_forward(&traded, Interval::M5, 0, 3 * step, None);
        assert_eq!(no_seed.len(), 1); // nothing before the first trade ever
        assert_eq!(no_seed[0].t, 2 * step);

        let seeded = carry_forward(&traded, Interval::M5, 0, 3 * step, Some(4.0));
        assert_eq!(seeded.len(), 3);
        assert_eq!(seeded[0], Bar { t: 0, o: 4.0, h: 4.0, l: 4.0, c: 4.0, v: 0.0 });
        assert_eq!(seeded[2].c, 5.0);
    }

    #[test]
    fn mid_gaps_carry_forward_and_leading_gap_respects_seed() {
        let step = Interval::M1.ms();
        let sampled = [SampledMid { t: 2 * step, m: 7.5 }];

        // Without a seed, nothing before the first sample ever.
        let no_seed = carry_forward_mids(&sampled, Interval::M1, 0, 4 * step, None);
        assert_eq!(no_seed.len(), 2);
        assert_eq!(no_seed[0], MidPoint { t: 2 * step, m: 7.5 });
        assert_eq!(no_seed[1], MidPoint { t: 3 * step, m: 7.5 }); // carried

        // With a seed, leading buckets carry it until the first sample.
        let seeded = carry_forward_mids(&sampled, Interval::M1, 0, 4 * step, Some(7.0));
        assert_eq!(seeded.len(), 4);
        assert_eq!(seeded[0], MidPoint { t: 0, m: 7.0 });
        assert_eq!(seeded[1].m, 7.0);
        assert_eq!(seeded[2].m, 7.5);
    }

    #[test]
    fn range_aligns_down_to_grid() {
        let step = Interval::M1.ms();
        let traded = [tb(0, 1.0, 1.0)];
        // from inside bucket 0 → bucket 0 still emitted.
        let bars = carry_forward(&traded, Interval::M1, step / 2, step, None);
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].t, 0);
    }
}
