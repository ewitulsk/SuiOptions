//! Per-stage latency model (doc 08 §6.3): one distribution per stage,
//! never a single global offset. Draws are deterministic — a seeded
//! SplitMix64 stream consumed in event order — so the same inputs and
//! seed give the same latencies. Archive rows carry no receive
//! timestamps, so every stage is `assumed = true` until the data room's
//! chain-inclusion / detection capture (doc 08 §3.2) calibrates it; the
//! flag is serialized with every run.

use serde::{Deserialize, Serialize};

/// Uniform on `[mean_ms − jitter_ms, mean_ms + jitter_ms]`, floored at 0.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct LatencyDist {
    pub mean_ms: i64,
    pub jitter_ms: i64,
    /// True when the parameters are a stated assumption rather than a
    /// calibration from captured timestamps.
    pub assumed: bool,
}

impl Default for LatencyDist {
    fn default() -> Self {
        Self { mean_ms: 0, jitter_ms: 0, assumed: true }
    }
}

impl LatencyDist {
    pub const fn fixed(mean_ms: i64) -> Self {
        Self { mean_ms, jitter_ms: 0, assumed: true }
    }
}

/// `[latency]` in the scenario: one entry per §6.3 stage.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct LatencyConfig {
    /// Market/oracle observation, on top of the oracle model's own
    /// publish latency (`oracle.latency_ms`).
    pub observation: LatencyDist,
    /// Strategy computation and quote response.
    pub strategy: LatencyDist,
    /// Customer acceptance (RFQ lifecycle; constant flow accepts at once).
    pub acceptance: LatencyDist,
    /// Venue submission (command → order at the venue).
    pub venue_submit: LatencyDist,
    /// Venue acknowledgement.
    pub venue_ack: LatencyDist,
    /// Venue cancel processing.
    pub venue_cancel: LatencyDist,
    /// Fill reporting (execution → the desk learns of it).
    pub venue_fill_report: LatencyDist,
    /// Sui transaction inclusion.
    pub sui_inclusion: LatencyDist,
    /// Indexer / fill detection.
    pub indexer_detection: LatencyDist,
    /// Seed of the latency draw stream.
    pub seed: u64,
}

impl Default for LatencyConfig {
    fn default() -> Self {
        Self {
            observation: LatencyDist::fixed(0),
            strategy: LatencyDist { mean_ms: 200, jitter_ms: 100, assumed: true },
            acceptance: LatencyDist::fixed(0),
            venue_submit: LatencyDist { mean_ms: 150, jitter_ms: 100, assumed: true },
            venue_ack: LatencyDist { mean_ms: 150, jitter_ms: 100, assumed: true },
            venue_cancel: LatencyDist { mean_ms: 150, jitter_ms: 100, assumed: true },
            venue_fill_report: LatencyDist { mean_ms: 200, jitter_ms: 100, assumed: true },
            sui_inclusion: LatencyDist { mean_ms: 1_500, jitter_ms: 1_000, assumed: true },
            indexer_detection: LatencyDist { mean_ms: 2_000, jitter_ms: 1_000, assumed: true },
            seed: 1,
        }
    }
}

impl LatencyConfig {
    /// Every stage at zero: the v0 synchronous replay.
    pub fn zero() -> Self {
        Self {
            observation: LatencyDist::fixed(0),
            strategy: LatencyDist::fixed(0),
            acceptance: LatencyDist::fixed(0),
            venue_submit: LatencyDist::fixed(0),
            venue_ack: LatencyDist::fixed(0),
            venue_cancel: LatencyDist::fixed(0),
            venue_fill_report: LatencyDist::fixed(0),
            sui_inclusion: LatencyDist::fixed(0),
            indexer_detection: LatencyDist::fixed(0),
            seed: 1,
        }
    }

    pub fn is_zero(&self) -> bool {
        self.stages().iter().all(|(_, d)| d.mean_ms == 0 && d.jitter_ms == 0)
    }

    fn stages(&self) -> [(LatencyStage, &LatencyDist); 9] {
        [
            (LatencyStage::Observation, &self.observation),
            (LatencyStage::Strategy, &self.strategy),
            (LatencyStage::Acceptance, &self.acceptance),
            (LatencyStage::VenueSubmit, &self.venue_submit),
            (LatencyStage::VenueAck, &self.venue_ack),
            (LatencyStage::VenueCancel, &self.venue_cancel),
            (LatencyStage::VenueFillReport, &self.venue_fill_report),
            (LatencyStage::SuiInclusion, &self.sui_inclusion),
            (LatencyStage::IndexerDetection, &self.indexer_detection),
        ]
    }

    fn dist(&self, stage: LatencyStage) -> &LatencyDist {
        self.stages().iter().find(|(s, _)| *s == stage).map(|(_, d)| *d).expect("every stage listed")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LatencyStage {
    Observation,
    Strategy,
    Acceptance,
    VenueSubmit,
    VenueAck,
    VenueCancel,
    VenueFillReport,
    SuiInclusion,
    IndexerDetection,
}

/// SplitMix64: tiny, seedable, and identical on every platform.
#[derive(Clone, Debug)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[derive(Clone, Debug)]
pub struct LatencyModel {
    cfg: LatencyConfig,
    rng: SplitMix64,
    draws: u64,
}

impl LatencyModel {
    pub fn new(cfg: LatencyConfig) -> Self {
        let rng = SplitMix64::new(cfg.seed);
        Self { cfg, rng, draws: 0 }
    }

    pub fn config(&self) -> &LatencyConfig {
        &self.cfg
    }

    /// One latency draw for `stage`, ms. A zero-width distribution never
    /// consumes randomness, so zero-latency runs draw nothing.
    pub fn draw(&mut self, stage: LatencyStage) -> i64 {
        let d = *self.cfg.dist(stage);
        if d.jitter_ms <= 0 {
            return d.mean_ms.max(0);
        }
        self.draws += 1;
        let u = self.rng.next_f64();
        let off = ((2.0 * u - 1.0) * d.jitter_ms as f64).round() as i64;
        (d.mean_ms + off).max(0)
    }

    pub fn draws(&self) -> u64 {
        self.draws
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_are_seeded_bounded_and_zero_free() {
        let mut a = LatencyModel::new(LatencyConfig::default());
        let mut b = LatencyModel::new(LatencyConfig::default());
        for _ in 0..1000 {
            let x = a.draw(LatencyStage::SuiInclusion);
            assert_eq!(x, b.draw(LatencyStage::SuiInclusion));
            assert!((500..=2500).contains(&x), "{x}");
        }
        assert_eq!(a.draws(), 1000);
        let mut z = LatencyModel::new(LatencyConfig::zero());
        assert_eq!(z.draw(LatencyStage::Strategy), 0);
        assert_eq!(z.draws(), 0);
        assert!(LatencyConfig::zero().is_zero());
        assert!(!LatencyConfig::default().is_zero());
        // A different seed is a different stream.
        let mut c = LatencyModel::new(LatencyConfig { seed: 2, ..LatencyConfig::default() });
        let same = (0..50).all(|_| c.draw(LatencyStage::VenueAck) == a.draw(LatencyStage::VenueAck));
        assert!(!same);
    }
}
