//! Engagement→airdrop-point conversion. Pure functions over config weights —
//! points are derived at read time, never stored, so re-tuning weights takes
//! effect immediately without a backfill.

use serde::Deserialize;

fn default_like_weight() -> f64 {
    1.0
}
fn default_reply_weight() -> f64 {
    2.0
}
fn default_retweet_weight() -> f64 {
    3.0
}
fn default_quote_weight() -> f64 {
    4.0
}
fn default_airdrop_rate() -> f64 {
    10.0
}
fn default_ambassador_multiplier() -> f64 {
    1.5
}

#[derive(Debug, Clone, Deserialize)]
pub struct PointsConfig {
    #[serde(default = "default_like_weight")]
    pub like_weight: f64,
    #[serde(default = "default_reply_weight")]
    pub reply_weight: f64,
    #[serde(default = "default_retweet_weight")]
    pub retweet_weight: f64,
    #[serde(default = "default_quote_weight")]
    pub quote_weight: f64,

    /// Airdrop points granted per engagement point.
    #[serde(default = "default_airdrop_rate")]
    pub airdrop_points_per_engagement_point: f64,
    /// Ambassadors' airdrop points are multiplied by this.
    #[serde(default = "default_ambassador_multiplier")]
    pub ambassador_multiplier: f64,
    /// Ambassador twitter handles (without `@`, case-insensitive).
    #[serde(default)]
    pub ambassadors: Vec<String>,
}

/// Summed engagement counters (per tweet or per author).
#[derive(Debug, Clone, Copy, Default)]
pub struct Engagement {
    pub likes: i64,
    pub retweets: i64,
    pub replies: i64,
    pub quotes: i64,
}

impl PointsConfig {
    pub fn is_ambassador(&self, handle: &str) -> bool {
        self.ambassadors.iter().any(|a| a.eq_ignore_ascii_case(handle))
    }

    pub fn engagement_points(&self, e: Engagement) -> f64 {
        e.likes as f64 * self.like_weight
            + e.replies as f64 * self.reply_weight
            + e.retweets as f64 * self.retweet_weight
            + e.quotes as f64 * self.quote_weight
    }

    /// Engagement points → airdrop points, with the ambassador multiplier.
    pub fn airdrop_points(&self, handle: &str, engagement_points: f64) -> f64 {
        let multiplier = if self.is_ambassador(handle) {
            self.ambassador_multiplier
        } else {
            1.0
        };
        engagement_points * self.airdrop_points_per_engagement_point * multiplier
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> PointsConfig {
        PointsConfig {
            like_weight: 1.0,
            reply_weight: 2.0,
            retweet_weight: 3.0,
            quote_weight: 4.0,
            airdrop_points_per_engagement_point: 10.0,
            ambassador_multiplier: 1.5,
            ambassadors: vec!["Alice".to_string()],
        }
    }

    #[test]
    fn weights_engagement() {
        let e = Engagement {
            likes: 10,
            replies: 3,
            retweets: 2,
            quotes: 1,
        };
        // 10*1 + 3*2 + 2*3 + 1*4 = 26
        assert_eq!(cfg().engagement_points(e), 26.0);
    }

    #[test]
    fn ambassador_matching_is_case_insensitive() {
        let c = cfg();
        assert!(c.is_ambassador("alice"));
        assert!(c.is_ambassador("ALICE"));
        assert!(!c.is_ambassador("bob"));
    }

    #[test]
    fn converts_to_airdrop_points_with_multiplier() {
        let c = cfg();
        assert_eq!(c.airdrop_points("bob", 26.0), 260.0);
        assert_eq!(c.airdrop_points("alice", 26.0), 390.0);
    }
}
