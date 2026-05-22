//! In-memory bucket-family registry.
//!
//! A "family" is one `(underlying_type, settlement_type, expiry_ms)` slice
//! of the protocol's bucket-state: all the strike rows that share an
//! expiry. The scheduler hydrates this from the indexer's `BucketCreated`
//! events on boot and follows the live stream forever — buckets it created
//! itself flow back the same way, so the registry is single-sourced and we
//! never have to reconcile "did my submit land?" by hand.
//!
//! Type-string canonicalization
//! ----------------------------
//!
//! `BucketCreated.asset_type` is a `TypeName` BCS-decoded into a string, of
//! the form `<package-address>::<module>::<Type>`. The package address may
//! be either full-padded (`0x0b75…`) or shortened depending on the source,
//! and `TokenInfo.coin_type` in `deployments.json` may use either
//! convention. We normalise both via `ObjectID::from_str(...)` on the
//! first segment before comparing, so a configured TUSDC matches the
//! same Coin regardless of which form the chain emitted.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use shared::protocol_types::events::{BucketCleaned, BucketCreated, ChainEvent, IndexedEvent};
use shared::protocol_types::ids::ObjectId;
use shared::protocol_types::messages::{IndexerStream, IndexerSubscribe};
use sui_types::base_types::ObjectID;

/// Canonical `(package, module, type)` triple. Used to match the indexer's
/// type-string against configured token coin types.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CanonicalType {
    pub package: ObjectID,
    pub module: String,
    pub type_name: String,
}

impl CanonicalType {
    pub fn parse(s: &str) -> Result<Self> {
        let mut parts = s.splitn(3, "::");
        let pkg = parts
            .next()
            .ok_or_else(|| anyhow!("malformed type string {s}"))?;
        let module = parts
            .next()
            .ok_or_else(|| anyhow!("type string missing module: {s}"))?;
        let type_name = parts
            .next()
            .ok_or_else(|| anyhow!("type string missing type name: {s}"))?;
        // Allow the inner `<>` generics to remain on the type name — only
        // bare Coin types ever flow through this path today, but it costs
        // nothing to be forgiving.
        let package = ObjectID::from_hex_literal(pkg)
            .or_else(|_| ObjectID::from_str(pkg))
            .with_context(|| format!("parsing package id in type string {s}"))?;
        Ok(Self {
            package,
            module: module.to_owned(),
            type_name: type_name.to_owned(),
        })
    }
}

/// One configured pair, with both halves canonicalised so live events
/// match without re-parsing.
#[derive(Debug, Clone)]
pub struct PairKey {
    pub underlying_symbol: String,
    pub settlement_symbol: String,
    pub underlying: CanonicalType,
    pub settlement: CanonicalType,
}

impl PairKey {
    pub fn matches_event(&self, ev: &BucketCreated) -> bool {
        Self::asset_matches(&self.underlying, &ev.asset_type.0)
            && Self::asset_matches(&self.settlement, &ev.settlement_type.0)
    }

    fn asset_matches(canon: &CanonicalType, raw: &str) -> bool {
        match CanonicalType::parse(raw) {
            Ok(other) => &other == canon,
            Err(_) => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct StrikeBucket {
    pub strike: u64,
    pub bucket_id: ObjectId,
}

/// State shared between the indexer-driven hydrator task and the tick
/// loop. Both threads read; only the hydrator writes.
#[derive(Debug, Default)]
struct RegistryInner {
    /// `(pair_index, expiry_ms)` → strikes. The Vec is kept sorted by
    /// strike for stable logging.
    families: BTreeMap<(usize, u64), Vec<StrikeBucket>>,
    /// Every bucket id we've ever heard of — lets `BucketCleaned` find
    /// which `(pair, expiry)` to drop the strike from without scanning.
    bucket_index: HashMap<ObjectId, (usize, u64)>,
    /// Highest sequence ingested.
    last_sequence: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Registry {
    inner: Arc<RwLock<RegistryInner>>,
}

#[derive(Debug, Clone)]
pub struct FamilySummary {
    pub expiry_ms: u64,
    pub strikes: Vec<StrikeBucket>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of every family for `pair_idx`, ordered by ascending expiry.
    pub fn families_for(&self, pair_idx: usize) -> Vec<FamilySummary> {
        let g = self.inner.read();
        g.families
            .range((pair_idx, 0)..=(pair_idx, u64::MAX))
            .map(|((_, expiry), strikes)| FamilySummary {
                expiry_ms: *expiry,
                strikes: strikes.clone(),
            })
            .collect()
    }

    /// Latest (highest-expiry) family for a pair, or None.
    pub fn latest_family(&self, pair_idx: usize) -> Option<FamilySummary> {
        let g = self.inner.read();
        g.families
            .range((pair_idx, 0)..=(pair_idx, u64::MAX))
            .next_back()
            .map(|((_, expiry), strikes)| FamilySummary {
                expiry_ms: *expiry,
                strikes: strikes.clone(),
            })
    }

    pub fn last_sequence(&self) -> u64 {
        self.inner.read().last_sequence
    }

    fn apply_created(&self, pair_idx: usize, ev: &BucketCreated) {
        let mut g = self.inner.write();
        let key = (pair_idx, ev.expiry_ms);
        let entry = g.families.entry(key).or_default();
        // Insert in strike-sorted order; reject exact duplicates so a
        // replayed snapshot is idempotent.
        let new = StrikeBucket {
            strike: ev.strike,
            bucket_id: ev.bucket_id,
        };
        if !entry.iter().any(|b| b.bucket_id == new.bucket_id) {
            match entry.binary_search_by_key(&new.strike, |b| b.strike) {
                Ok(i) | Err(i) => entry.insert(i, new),
            }
        }
        g.bucket_index.insert(ev.bucket_id, key);
    }

    fn apply_cleaned(&self, ev: &BucketCleaned) {
        let mut g = self.inner.write();
        if let Some(key) = g.bucket_index.remove(&ev.bucket_id) {
            if let Some(strikes) = g.families.get_mut(&key) {
                strikes.retain(|b| b.bucket_id != ev.bucket_id);
                if strikes.is_empty() {
                    g.families.remove(&key);
                }
            }
        }
    }

    fn note_sequence(&self, seq: u64) {
        let mut g = self.inner.write();
        if seq > g.last_sequence {
            g.last_sequence = seq;
        }
    }
}

/// Subscribe to the indexer fanout and stream events into `registry`.
/// Reconnects forever; the indexer's `Snapshot` catch-up handles missed
/// sequences during downtime.
pub async fn run_subscriber(
    url: String,
    registry: Registry,
    pairs: Arc<Vec<PairKey>>,
) -> Result<()> {
    loop {
        let after = registry.last_sequence();
        match tokio_tungstenite::connect_async(&url).await {
            Ok((mut ws, _)) => {
                info!(%url, after_sequence = after, "indexer subscriber connected");
                let sub = IndexerSubscribe::Subscribe { after_sequence: after };
                if let Err(e) = ws.send(Message::Text(serde_json::to_string(&sub)?)).await {
                    warn!(error = %e, "indexer subscribe failed; reconnecting");
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
                while let Some(frame) = ws.next().await {
                    let frame = match frame {
                        Ok(f) => f,
                        Err(e) => {
                            warn!(error = %e, "indexer ws read failed; reconnecting");
                            break;
                        }
                    };
                    let text = match frame {
                        Message::Text(t) => t,
                        Message::Binary(b) => match String::from_utf8(b) {
                            Ok(s) => s,
                            Err(_) => continue,
                        },
                        Message::Close(_) => break,
                        _ => continue,
                    };
                    match serde_json::from_str::<IndexerStream>(&text) {
                        Ok(IndexerStream::Snapshot { payload }) => {
                            debug!(events = payload.events.len(), "snapshot");
                            for e in &payload.events {
                                ingest(&registry, &pairs, e);
                            }
                            registry.note_sequence(payload.latest_sequence);
                        }
                        Ok(IndexerStream::Event { payload }) => {
                            ingest(&registry, &pairs, &payload);
                        }
                        Ok(IndexerStream::Heartbeat { latest_sequence }) => {
                            registry.note_sequence(latest_sequence);
                        }
                        Err(e) => {
                            warn!(error = %e, frame = %text, "bad indexer frame");
                        }
                    }
                }
            }
            Err(e) => warn!(error = %e, %url, "indexer connect failed"),
        }
        sleep(Duration::from_secs(1)).await;
    }
}

fn ingest(registry: &Registry, pairs: &[PairKey], env: &IndexedEvent) {
    match &env.event {
        ChainEvent::BucketCreated(ev) => {
            if let Some(idx) = pairs.iter().position(|p| p.matches_event(ev)) {
                registry.apply_created(idx, ev);
            } else {
                // Drop silently — chain has buckets for assets we don't care about.
            }
        }
        ChainEvent::BucketCleaned(ev) => registry.apply_cleaned(ev),
        _ => {}
    }
    registry.note_sequence(env.sequence);
}

/// Print every family the registry currently knows about, grouped by pair.
/// Used at boot under `--dry-run` to confirm the indexer hydration matched
/// the operator's expectation.
pub fn log_registry(registry: &Registry, pairs: &[PairKey]) {
    for (idx, pair) in pairs.iter().enumerate() {
        let families = registry.families_for(idx);
        if families.is_empty() {
            info!(
                pair = %format!("{}/{}", pair.underlying_symbol, pair.settlement_symbol),
                "no families on chain yet"
            );
            continue;
        }
        for fam in families {
            let strikes: Vec<u64> = fam.strikes.iter().map(|b| b.strike).collect();
            let ids: BTreeSet<String> = fam
                .strikes
                .iter()
                .map(|b| b.bucket_id.to_hex())
                .collect();
            info!(
                pair = %format!("{}/{}", pair.underlying_symbol, pair.settlement_symbol),
                expiry_ms = fam.expiry_ms,
                strikes = ?strikes,
                buckets = ?ids,
                "family"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::protocol_types::asset::AssetType;
    use shared::protocol_types::events::{BucketCleaned, BucketCreated};
    use shared::protocol_types::ids::ObjectId;

    fn pair_key() -> PairKey {
        PairKey {
            underlying_symbol: "TBTC".into(),
            settlement_symbol: "TUSDC".into(),
            underlying: CanonicalType::parse(
                "0x0b756179b7ae9efea2fdfb805308443bab763605459b92947616e0a04136d843::tbtc::TBTC",
            )
            .unwrap(),
            settlement: CanonicalType::parse(
                "0x0b756179b7ae9efea2fdfb805308443bab763605459b92947616e0a04136d843::tusdc::TUSDC",
            )
            .unwrap(),
        }
    }

    #[test]
    fn matches_short_package_form() {
        // ObjectID::from_hex_literal pads leading zeros up to 32 bytes, so
        // a chain that emits "0x2::sui::SUI" parses to the same canonical
        // type as "0x000…02::sui::SUI". This is the form everyone trips on
        // first; the test just locks the round-trip.
        let short = CanonicalType::parse("0x2::sui::SUI").unwrap();
        let long = CanonicalType::parse(
            "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI",
        )
        .unwrap();
        assert_eq!(short, long);
    }

    #[test]
    fn ingests_created_then_cleaned() {
        let r = Registry::new();
        let p = pair_key();

        let ev = BucketCreated {
            bucket_id: ObjectId::new([0x11; 32]),
            asset_type: AssetType::new(
                "0x0b756179b7ae9efea2fdfb805308443bab763605459b92947616e0a04136d843::tbtc::TBTC",
            ),
            settlement_type: AssetType::new(
                "0x0b756179b7ae9efea2fdfb805308443bab763605459b92947616e0a04136d843::tusdc::TUSDC",
            ),
            expiry_ms: 1_700_000_000_000,
            strike: 500,
        };
        assert!(p.matches_event(&ev));
        r.apply_created(0, &ev);
        let latest = r.latest_family(0).unwrap();
        assert_eq!(latest.expiry_ms, 1_700_000_000_000);
        assert_eq!(latest.strikes.len(), 1);

        // Idempotent replay.
        r.apply_created(0, &ev);
        assert_eq!(r.latest_family(0).unwrap().strikes.len(), 1);

        r.apply_cleaned(&BucketCleaned {
            bucket_id: ev.bucket_id,
        });
        assert!(r.latest_family(0).is_none());
    }

    #[test]
    fn unknown_pair_is_ignored() {
        let r = Registry::new();
        let p = pair_key();
        let ev = BucketCreated {
            bucket_id: ObjectId::new([0x22; 32]),
            asset_type: AssetType::new("0x2::sui::SUI"),
            settlement_type: AssetType::new(
                "0x0b756179b7ae9efea2fdfb805308443bab763605459b92947616e0a04136d843::tusdc::TUSDC",
            ),
            expiry_ms: 1,
            strike: 1,
        };
        assert!(!p.matches_event(&ev));
        // mirror what `ingest` does: skip apply_created when no pair matches.
        assert!(r.latest_family(0).is_none());
    }

    #[test]
    fn highest_expiry_wins_as_latest() {
        let r = Registry::new();
        let mk = |strike: u64, expiry: u64| BucketCreated {
            bucket_id: ObjectId::new([strike as u8; 32]),
            asset_type: AssetType::new(
                "0x0b756179b7ae9efea2fdfb805308443bab763605459b92947616e0a04136d843::tbtc::TBTC",
            ),
            settlement_type: AssetType::new(
                "0x0b756179b7ae9efea2fdfb805308443bab763605459b92947616e0a04136d843::tusdc::TUSDC",
            ),
            expiry_ms: expiry,
            strike,
        };
        r.apply_created(0, &mk(400, 1_000));
        r.apply_created(0, &mk(425, 1_000));
        r.apply_created(0, &mk(450, 2_000));
        r.apply_created(0, &mk(475, 2_000));
        let latest = r.latest_family(0).unwrap();
        assert_eq!(latest.expiry_ms, 2_000);
        assert_eq!(latest.strikes.len(), 2);
        assert_eq!(latest.strikes[0].strike, 450);
        assert_eq!(latest.strikes[1].strike, 475);
    }
}
