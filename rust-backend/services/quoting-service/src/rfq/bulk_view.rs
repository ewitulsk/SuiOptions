//! Bulk-view RFQ orchestration.
//!
//! Retail tiles want an indicative premium for *every* strike in a series
//! without firing a signable RFQ (and thus a reservation) per tile. The
//! bulk-view path broadcasts one unsigned `BulkViewRFQBroadcast` covering many
//! buckets to the MMs that opted in, averages their (unsigned) premiums, and
//! caches the result per `(bucket_id, write_amount)`.
//!
//! Serving is **stale-while-revalidate**:
//!
//! - **Fresh** (cache age < TTL): returned from cache, no MM traffic.
//! - **Stale** (cache age ≥ TTL): the stale value is returned immediately and
//!   a background refresh re-broadcasts to MMs (single-flight per key).
//! - **Cold** (no cache entry): fetched synchronously this call so the first
//!   load still gets a value within the response window.
//!
//! Nothing here reserves balance, consumes a nonce, or touches reputation —
//! these premiums are display-only and never reach the chain.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, trace};

use protocol_types::ids::ObjectId;
use protocol_types::messages::{
    BulkViewBucket, BulkViewPremium, BulkViewRfqBroadcastPayload, ServiceToMm,
};
use protocol_types::sides::Side;

use indexer_graphql::Bucket;

use crate::state::{AppState, BulkViewCacheEntry, MmResponse};

/// Arithmetic mean of `premiums`, settlement smallest-units. Returns 0 for an
/// empty slice (the caller treats a zero-response bucket as "no value" and
/// doesn't cache it). `u128` accumulator so a long list can't overflow.
pub fn mean_premium(premiums: &[u64]) -> u64 {
    if premiums.is_empty() {
        return 0;
    }
    let sum: u128 = premiums.iter().map(|p| *p as u128).sum();
    (sum / premiums.len() as u128) as u64
}

/// Drive a bulk-view request to a list of per-bucket indicative premiums.
///
/// `buckets` are resolved (and invalidated/unknown ones already dropped) by
/// the caller. `now_ms` is the wall clock used for TTL/staleness math.
pub async fn orchestrate_bulk_view(
    state: Arc<AppState>,
    side: Side,
    buckets: Vec<Bucket>,
    write_amount: u64,
    rfq_window: Duration,
    cache_ttl: Duration,
    now_ms: u64,
) -> Vec<BulkViewPremium> {
    let ttl_ms = cache_ttl.as_millis() as u64;
    let requested: Vec<ObjectId> = buckets.iter().map(|b| b.bucket_id).collect();

    // Partition against the cache.
    let mut cold: Vec<Bucket> = Vec::new();
    let mut stale: Vec<Bucket> = Vec::new();
    for b in &buckets {
        match state.bulk_view_cache.get(&(b.bucket_id, write_amount)) {
            Some(e) if now_ms.saturating_sub(e.cached_at_ms) < ttl_ms => { /* fresh */ }
            Some(_) => stale.push(b.clone()),
            None => cold.push(b.clone()),
        }
    }

    let has_cold = !cold.is_empty();
    let mut need_refresh = cold;
    need_refresh.append(&mut stale);

    if !need_refresh.is_empty() {
        if has_cold {
            // Block on the refresh so cold buckets get a value this call.
            refresh_buckets(
                Arc::clone(&state),
                side,
                need_refresh,
                write_amount,
                rfq_window,
                now_ms,
            )
            .await;
        } else {
            // Only stale: serve cache now, refresh in the background.
            let st = Arc::clone(&state);
            tokio::spawn(async move {
                refresh_buckets(st, side, need_refresh, write_amount, rfq_window, now_ms).await;
            });
        }
    }

    build_response(&state, &requested, write_amount, now_ms, ttl_ms)
}

/// Read the current cache state into the wire response. Buckets with no cache
/// entry are omitted (their tile shows the placeholder).
fn build_response(
    state: &AppState,
    requested: &[ObjectId],
    write_amount: u64,
    now_ms: u64,
    ttl_ms: u64,
) -> Vec<BulkViewPremium> {
    let mut out = Vec::with_capacity(requested.len());
    for bucket_id in requested {
        if let Some(e) = state.bulk_view_cache.get(&(*bucket_id, write_amount)) {
            let age = now_ms.saturating_sub(e.cached_at_ms);
            out.push(BulkViewPremium {
                bucket_id: *bucket_id,
                premium: e.premium,
                mm_count: e.mm_count,
                stale: age >= ttl_ms,
                cache_age_ms: age,
            });
        }
    }
    out
}

/// Broadcast a bulk-view RFQ for `buckets` to opted-in MMs, average the
/// responses, and write the cache. Single-flight per `(bucket, amount)` key:
/// any bucket already being refreshed by another task is skipped here (its
/// value comes from that task / the cache).
async fn refresh_buckets(
    state: Arc<AppState>,
    side: Side,
    buckets: Vec<Bucket>,
    write_amount: u64,
    window: Duration,
    now_ms: u64,
) {
    // Claim single-flight slots; only refresh the ones we win.
    let mut claimed: Vec<Bucket> = Vec::with_capacity(buckets.len());
    for b in buckets {
        if state.try_claim_bulk_view_refresh((b.bucket_id, write_amount)) {
            claimed.push(b);
        }
    }
    if claimed.is_empty() {
        return;
    }

    // Release every claim on the way out, whatever happens.
    let _guard = ClaimGuard {
        state: Arc::clone(&state),
        keys: claimed.iter().map(|b| (b.bucket_id, write_amount)).collect(),
    };

    let mms = state.mms.all_for_bulk_view(side.counterparty_mm());
    if mms.is_empty() {
        debug!("bulk-view refresh: no opted-in mms; leaving cache untouched");
        return;
    }

    let deadline_ms = now_ms.saturating_add(window.as_millis() as u64);
    let wire_buckets: Vec<BulkViewBucket> = claimed
        .iter()
        .map(|b| BulkViewBucket {
            bucket_id: b.bucket_id,
            strike: b.strike,
            strike_scale: b.strike_scale,
            expiry_ms: b.expiry_ms,
        })
        .collect();

    // Route MM responses through the shared pending_rfqs table under a fresh
    // routing id (independent of any retail request_id).
    let routing_id = uuid::Uuid::new_v4().to_string();
    let (tx, mut rx) = mpsc::channel::<MmResponse>(mms.len().max(8));
    state.pending_rfqs.insert(routing_id.clone(), tx);

    let mut expected = 0usize;
    for mm in &mms {
        let frame = ServiceToMm::BulkViewRFQBroadcast {
            request_id: routing_id.clone(),
            payload: BulkViewRfqBroadcastPayload {
                write_amount,
                side,
                deadline_ms,
                buckets: wire_buckets.clone(),
            },
        };
        match mm.tx.try_send(frame) {
            Ok(()) => expected += 1,
            Err(e) => {
                debug!(mm = %mm.account_id, error = %e, "dropping bulk-view broadcast: mm channel full or closed");
            }
        }
    }

    // Collect until every expected MM answered or the window elapses.
    let mut by_bucket: HashMap<ObjectId, Vec<u64>> = HashMap::new();
    if expected > 0 {
        let mut remaining = expected;
        let _ = timeout(window, async {
            while let Some(r) = rx.recv().await {
                if let MmResponse::BulkView(mm, payload) = r {
                    trace!(mm = %mm, premiums = payload.premiums.len(), "bulk-view response");
                    for p in payload.premiums {
                        by_bucket.entry(p.bucket_id).or_default().push(p.premium);
                    }
                    remaining -= 1;
                    if remaining == 0 {
                        break;
                    }
                }
            }
        })
        .await;
    }
    state.pending_rfqs.remove(&routing_id);

    // Average and cache. A bucket no MM priced is left untouched so the next
    // request retries it rather than caching a misleading zero.
    let mut cached = 0usize;
    for b in &claimed {
        if let Some(premiums) = by_bucket.get(&b.bucket_id) {
            if !premiums.is_empty() {
                state.bulk_view_cache.insert(
                    (b.bucket_id, write_amount),
                    BulkViewCacheEntry {
                        premium: mean_premium(premiums),
                        mm_count: premiums.len() as u32,
                        cached_at_ms: now_ms,
                    },
                );
                cached += 1;
            }
        }
    }
    debug!(claimed = claimed.len(), cached, mms = mms.len(), "bulk-view refresh complete");
}

/// Releases the single-flight claims for a refresh batch on drop.
struct ClaimGuard {
    state: Arc<AppState>,
    keys: Vec<(ObjectId, u64)>,
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        for k in &self.keys {
            self.state.release_bulk_view_refresh(*k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use protocol_types::asset::AssetType;

    fn test_state() -> Arc<AppState> {
        Arc::new(AppState::with_global_rfq_cap(
            256,
            "http://127.0.0.1:1/graphql".into(),
        ))
    }

    fn mk_bucket(tag: u8) -> Bucket {
        Bucket {
            bucket_id: ObjectId::new([tag; 32]),
            asset_type: AssetType::new("BTC"),
            settlement_type: AssetType::new("USDC"),
            strike: 50,
            strike_scale: 0,
            expiry_ms: 1_000_000,
            total_written: 0,
            exercise_cursor: 0,
            cleaned: false,
            invalidated: false,
        }
    }

    #[test]
    fn mean_of_empty_is_zero() {
        assert_eq!(mean_premium(&[]), 0);
    }

    #[test]
    fn mean_averages_and_floors() {
        assert_eq!(mean_premium(&[100, 200, 300]), 200);
        // 100 + 101 = 201 / 2 = 100 (floor).
        assert_eq!(mean_premium(&[100, 101]), 100);
    }

    #[tokio::test]
    async fn fresh_cache_hit_returns_without_mms() {
        let state = test_state();
        let b = mk_bucket(0x01);
        let now = 1_000_000;
        state.bulk_view_cache.insert(
            (b.bucket_id, 100),
            BulkViewCacheEntry {
                premium: 4242,
                mm_count: 2,
                cached_at_ms: now,
            },
        );
        // No MMs registered; a fresh hit must not need them.
        let out = orchestrate_bulk_view(
            Arc::clone(&state),
            Side::Writer,
            vec![b.clone()],
            100,
            Duration::from_millis(50),
            Duration::from_secs(30),
            now + 5_000, // 5s old, < 30s TTL
        )
        .await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].premium, 4242);
        assert!(!out[0].stale);
        assert_eq!(out[0].cache_age_ms, 5_000);
    }

    #[tokio::test]
    async fn cold_bucket_with_no_mms_is_omitted_and_does_not_block() {
        let state = test_state();
        let b = mk_bucket(0x02);
        let start = std::time::Instant::now();
        let out = orchestrate_bulk_view(
            Arc::clone(&state),
            Side::Writer,
            vec![b],
            100,
            Duration::from_millis(50),
            Duration::from_secs(30),
            2_000_000,
        )
        .await;
        // No MMs → nothing cached → bucket omitted, and we returned promptly.
        assert!(out.is_empty());
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn stale_entry_served_immediately_and_flagged() {
        let state = test_state();
        let b = mk_bucket(0x03);
        let cached_at = 1_000_000;
        state.bulk_view_cache.insert(
            (b.bucket_id, 100),
            BulkViewCacheEntry {
                premium: 999,
                mm_count: 1,
                cached_at_ms: cached_at,
            },
        );
        // 40s later: past the 30s TTL → stale, but still returned (no MMs, so
        // the spawned background refresh is a no-op).
        let out = orchestrate_bulk_view(
            Arc::clone(&state),
            Side::Writer,
            vec![b],
            100,
            Duration::from_millis(50),
            Duration::from_secs(30),
            cached_at + 40_000,
        )
        .await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].premium, 999);
        assert!(out[0].stale);
        assert_eq!(out[0].cache_age_ms, 40_000);
    }
}
