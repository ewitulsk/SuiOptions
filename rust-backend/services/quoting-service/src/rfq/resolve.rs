//! Spec → bucket resolution.
//!
//! Under spec-bound quoting an RFQ names economics, not an object, and the
//! bucket for those economics may not exist yet. That is the normal case on a
//! freshly-listed strike, not an error — so this returns "does it exist, and
//! if so what is its state" rather than succeeding or failing.
//!
//! Existence is monotone (a bucket never un-exists), so a hit is cached for
//! the process lifetime and a miss is cached only briefly: the miss→hit
//! transition happens the moment somebody's fill creates the bucket, and the
//! only cost of noticing late is that the `invalidated` refusal lags by at
//! most the negative TTL.

use std::time::{Duration, Instant};

use anyhow::Result;
use dashmap::DashMap;

use indexer_graphql::{Bucket, IndexerClient};
use protocol_types::asset::AssetType;
use protocol_types::bucket_spec::{normalize_strike, BucketSpec};

/// How long a "no bucket for this spec yet" answer is trusted.
const NEGATIVE_TTL: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub enum Resolved {
    /// A bucket exists on chain for this spec.
    Exists(Box<Bucket>),
    /// No bucket yet — quotable, and the taker's own transaction creates it.
    Pending,
}

impl Resolved {
    pub fn bucket(&self) -> Option<&Bucket> {
        match self {
            Self::Exists(b) => Some(b),
            Self::Pending => None,
        }
    }

    /// Written size already queued ahead of a new write. Zero for a bucket
    /// that does not exist yet, which is exactly right — nothing is ahead.
    pub fn total_written(&self) -> u128 {
        self.bucket().map(|b| b.total_written).unwrap_or(0)
    }
}

pub struct SpecResolver {
    hits: DashMap<BucketSpec, Bucket>,
    misses: DashMap<BucketSpec, Instant>,
}

impl Default for SpecResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl SpecResolver {
    pub fn new() -> Self {
        Self {
            hits: DashMap::new(),
            misses: DashMap::new(),
        }
    }

    /// Resolve `spec` against the indexer.
    ///
    /// A transport failure is an error, not a `Pending`: quoting on the
    /// assumption that a bucket does not exist would skip the `invalidated`
    /// check on one that does.
    pub async fn resolve(&self, indexer: &IndexerClient, spec: &BucketSpec) -> Result<Resolved> {
        if let Some(hit) = self.hits.get(spec) {
            return Ok(Resolved::Exists(Box::new(hit.clone())));
        }
        if let Some(at) = self.misses.get(spec) {
            if at.elapsed() < NEGATIVE_TTL {
                return Ok(Resolved::Pending);
            }
        }

        // Narrow the query to the pair + expiry, then match the normalized
        // strike and kind locally.
        //
        // The filters string-match the indexer's stored chain-form
        // `TypeName`s, and `BucketSpec` already holds chain form — feeding it
        // the 0x-prefixed canonical form is the documented trap that made
        // `/buckets/spec` report `exists: false` for a live bucket (#479).
        let candidates = indexer
            .buckets(
                /* active_only */ false,
                Some(&AssetType::new(spec.asset.clone())),
                Some(&AssetType::new(spec.settlement.clone())),
                Some(spec.expiry_ms),
            )
            .await?;

        let hit = candidates.into_iter().find(|b| {
            (b.option_kind == "put") == spec.is_put
                && !b.cleaned
                && normalize_strike(b.strike, b.strike_scale).ok() == Some((spec.sig, spec.exp))
        });

        match hit {
            Some(b) => {
                self.misses.remove(spec);
                self.hits.insert(spec.clone(), b.clone());
                Ok(Resolved::Exists(Box::new(b)))
            }
            None => {
                self.misses.insert(spec.clone(), Instant::now());
                Ok(Resolved::Pending)
            }
        }
    }

    /// Drop a cached hit — used when a bucket's mutable state (written,
    /// invalidated) must be re-read rather than served from the immutable-part
    /// cache.
    pub fn invalidate(&self, spec: &BucketSpec) {
        self.hits.remove(spec);
    }

    pub fn cached_len(&self) -> usize {
        self.hits.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(is_put: bool) -> BucketSpec {
        BucketSpec::new("0x9::a::A", "0x9::b::B", 60_000, 50_000, 0, is_put).unwrap()
    }

    /// Negative answers are cached, but only briefly — a bucket that appears
    /// between two RFQs must be seen.
    #[test]
    fn negative_ttl_is_short_and_positive_is_permanent() {
        assert!(NEGATIVE_TTL <= Duration::from_secs(10));
    }

    /// Call and put specs at the same pair/expiry/strike are distinct keys.
    /// If they collided, a put RFQ could be served a call bucket's state.
    #[test]
    fn call_and_put_specs_are_distinct_cache_keys() {
        let r = SpecResolver::new();
        assert_ne!(spec(false), spec(true));
        r.hits.insert(spec(false), fake_bucket());
        assert!(r.hits.get(&spec(true)).is_none());
    }

    fn fake_bucket() -> Bucket {
        Bucket {
            bucket_id: protocol_types::ids::ObjectId::new([0x01; 32]),
            asset_type: AssetType::new("a"),
            settlement_type: AssetType::new("b"),
            call_type: AssetType::new("c"),
            strike: 50_000,
            strike_scale: 0,
            expiry_ms: 60_000,
            total_written: 0,
            exercise_cursor: 0,
            cleaned: false,
            invalidated: false,
            option_kind: "call".into(),
            deepbook_pool_id: None,
            exchange_market_id: None,
        }
    }
}
