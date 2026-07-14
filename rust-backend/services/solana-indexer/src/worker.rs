//! LaserStream ingestion worker.
//!
//! One subscription at `confirmed` commitment carries three filters:
//!   - transactions touching any of the three program ids (the events);
//!   - slot status updates (unfiltered by commitment — we consume both the
//!     `confirmed` ticks that drive batching and the `finalized` ticks
//!     that drive the reorg watermark, plus `dead` fork signals);
//!   - block meta (block_time for event timestamps).
//!
//! Batching mirrors the Sui checkpoint worker: transactions buffer per
//! slot, and the slot's `confirmed` notification — which LaserStream
//! delivers after all of the slot's messages — flushes the batch in one DB
//! transaction.
//!
//! Reorg handling (the two-tier design):
//!   - Rows land at `confirmed` (sub-second) tagged with their slot.
//!   - `finalized` slot ticks advance `indexer_progress.finalized_slot`;
//!     everything at or below it is immutable truth.
//!   - A provisional slot the finalized chain skipped (its notification
//!     never arrives while the watermark passes it) or a `SLOT_DEAD`
//!     notification means a fork: evict the slot's events and rebuild the
//!     views. At `confirmed` commitment this is a backstop that has never
//!     fired on mainnet — but the money math must not depend on that.
//!
//! Resume: the caller passes `from_slot = finalized_slot + 1` so replay
//! re-validates every provisional slot after a restart (replayed events
//! dedup on `(signature, inner_ix_index)`); LaserStream's SDK tracks slots
//! internally across reconnects (`replay: true`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use helius_laserstream::grpc::{
    subscribe_update::UpdateOneof, CommitmentLevel, SlotStatus, SubscribeRequest,
    SubscribeRequestFilterBlocksMeta, SubscribeRequestFilterSlots,
    SubscribeRequestFilterTransactions,
};
use helius_laserstream::{subscribe, LaserstreamConfig};
use tracing::{debug, error, info, warn};

use crate::db::{PendingEvent, Repo, SlotBatch};
use crate::decode::{extract_events, ProgramSet};
use crate::progress::ProgressState;

/// A finalized watermark this far past a provisional slot with no
/// finalized notification for it means the slot was forked away. Finalized
/// notifications arrive in order, so any margin > 0 is already generous.
const FORK_GRACE_SLOTS: u64 = 32;

/// How long without any stream message before the stall alert fires.
/// Solana confirms slots ~2-3/sec; devnet the same.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Bound on the per-slot bookkeeping maps.
const PRUNE_DEPTH: u64 = 10_000;

pub struct IngestOptions {
    pub endpoint: String,
    pub api_key: String,
    pub programs: ProgramSet,
    /// Base58 forms for the transaction filter.
    pub program_ids: Vec<String>,
    /// Resume point (`finalized_slot + 1`), `None` to tail from the tip.
    pub from_slot: Option<u64>,
    /// Slots above the watermark that already hold events (from a prior
    /// run) — the reconciler re-validates them via replayed notifications.
    pub initial_provisional: Vec<i64>,
}

pub async fn run(opts: IngestOptions, repo: Repo, progress: Arc<ProgressState>) -> Result<()> {
    let request = build_request(&opts);
    let config = LaserstreamConfig::new(opts.endpoint.clone(), opts.api_key.clone());
    info!(
        endpoint = %redact(&opts.endpoint),
        from_slot = ?opts.from_slot,
        programs = ?opts.program_ids,
        "subscribing to LaserStream"
    );

    let (stream, _handle) = subscribe(config, request);
    tokio::pin!(stream);

    let mut state = WorkerState::new(&opts);
    loop {
        let item = match tokio::time::timeout(STALL_TIMEOUT, stream.next()).await {
            Ok(item) => item,
            Err(_) => {
                error!(
                    alert_id = "solana-indexer-stream-stalled",
                    timeout_secs = STALL_TIMEOUT.as_secs(),
                    "no LaserStream message within the stall window"
                );
                continue;
            }
        };
        let Some(item) = item else {
            // The SDK exhausts its internal reconnect budget before ending
            // the stream — treat as fatal and let the process restart.
            anyhow::bail!("LaserStream stream ended");
        };
        match item {
            Ok(update) => {
                if let Some(oneof) = update.update_oneof {
                    state.handle(oneof, &repo, &progress)?;
                }
            }
            Err(e) => {
                // Transient errors are retried inside the SDK; ones that
                // surface here are worth eyes but not a crash-loop.
                error!(alert_id = "solana-indexer-stream-error", error = %e, "LaserStream error");
            }
        }
    }
}

fn build_request(opts: &IngestOptions) -> SubscribeRequest {
    let mut request = SubscribeRequest {
        commitment: Some(CommitmentLevel::Confirmed as i32),
        from_slot: opts.from_slot,
        ..Default::default()
    };
    request.transactions.insert(
        "protocol".to_string(),
        SubscribeRequestFilterTransactions {
            vote: Some(false),
            failed: Some(false),
            account_include: opts.program_ids.clone(),
            ..Default::default()
        },
    );
    request.slots.insert(
        "slots".to_string(),
        SubscribeRequestFilterSlots {
            // All statuses: confirmed drives batching, finalized the
            // watermark, dead the fork backstop.
            filter_by_commitment: Some(false),
            interslot_updates: Some(false),
        },
    );
    request.blocks_meta.insert(
        "meta".to_string(),
        SubscribeRequestFilterBlocksMeta::default(),
    );
    request
}

/// Strip the query/key portion of the endpoint for logging.
fn redact(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

struct WorkerState {
    programs: ProgramSet,
    /// Transactions buffered per slot until its confirmed notification.
    pending: BTreeMap<u64, Vec<PendingEvent>>,
    /// slot → block_time (ms) from block-meta updates.
    block_time_ms: BTreeMap<u64, i64>,
    /// Slots whose finalized notification we've seen.
    finalized_seen: BTreeSet<u64>,
    /// Applied slots that hold events and aren't validated as finalized
    /// yet — the set the reconciler watches.
    provisional: BTreeSet<u64>,
    last_confirmed: u64,
}

impl WorkerState {
    fn new(opts: &IngestOptions) -> Self {
        Self {
            programs: opts.programs.clone(),
            pending: BTreeMap::new(),
            block_time_ms: BTreeMap::new(),
            finalized_seen: BTreeSet::new(),
            provisional: opts.initial_provisional.iter().map(|s| *s as u64).collect(),
            last_confirmed: 0,
        }
    }

    fn handle(
        &mut self,
        oneof: UpdateOneof,
        repo: &Repo,
        progress: &Arc<ProgressState>,
    ) -> Result<()> {
        match oneof {
            UpdateOneof::Transaction(t) => {
                let Some(info) = &t.transaction else {
                    return Ok(());
                };
                let events = extract_events(info, &self.programs);
                if events.is_empty() {
                    return Ok(());
                }
                debug!(
                    slot = t.slot,
                    count = events.len(),
                    "decoded protocol events"
                );
                if t.slot <= self.last_confirmed {
                    // Straggler behind its slot notification (shouldn't
                    // happen per LaserStream ordering; harmless if it does
                    // — the batch is idempotent).
                    warn!(
                        slot = t.slot,
                        "transaction arrived after its slot was applied"
                    );
                    self.apply_batch(t.slot, events, repo)?;
                } else {
                    self.pending.entry(t.slot).or_default().extend(events);
                }
                Ok(())
            }
            UpdateOneof::BlockMeta(m) => {
                if let Some(bt) = &m.block_time {
                    self.block_time_ms.insert(m.slot, bt.timestamp * 1000);
                }
                Ok(())
            }
            UpdateOneof::Slot(s) => match s.status() {
                SlotStatus::SlotConfirmed => self.on_confirmed(s.slot, repo, progress),
                SlotStatus::SlotFinalized => self.on_finalized(s.slot, repo, progress),
                SlotStatus::SlotDead => self.on_dead(s.slot, repo),
                _ => Ok(()),
            },
            // Pings and filter echoes carry nothing to persist.
            _ => Ok(()),
        }
    }

    /// A slot reached `confirmed`: flush every buffered slot at or below
    /// it (in order), then advance the progress cursor.
    fn on_confirmed(
        &mut self,
        slot: u64,
        repo: &Repo,
        progress: &Arc<ProgressState>,
    ) -> Result<()> {
        let due: Vec<u64> = self.pending.range(..=slot).map(|(s, _)| *s).collect();
        for s in due {
            let events = self.pending.remove(&s).unwrap_or_default();
            self.apply_batch(s, events, repo)?;
        }
        self.last_confirmed = self.last_confirmed.max(slot);
        // Advance past empty slots too, so a restart doesn't rescan them.
        repo.advance_slot(slot as i64)
            .with_context(|| format!("advancing progress to slot {slot}"))?;
        progress.record_slot(slot);
        metrics::gauge!("solana_indexer_slot_height").set(slot as f64);
        self.prune(slot);
        Ok(())
    }

    fn apply_batch(&mut self, slot: u64, events: Vec<PendingEvent>, repo: &Repo) -> Result<()> {
        for ev in &events {
            metrics::counter!(
                "solana_indexer_events_decoded_total",
                "event_type" => ev.event.tag(),
            )
            .increment(1);
        }
        let timestamp_ms = self
            .block_time_ms
            .get(&slot)
            .copied()
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let batch = SlotBatch {
            slot: slot as i64,
            timestamp_ms,
            events,
        };
        let apply_start = std::time::Instant::now();
        let inserted = repo
            .apply_slot(&batch)
            .with_context(|| format!("persisting slot {slot}"))?;
        metrics::histogram!("solana_indexer_slot_apply_duration_seconds")
            .record(apply_start.elapsed().as_secs_f64());
        debug!(slot, inserted, "slot persisted");
        self.provisional.insert(slot);
        Ok(())
    }

    /// A slot reached `finalized`: advance the watermark and validate the
    /// provisional set. A provisional slot the watermark passed without a
    /// finalized notification (plus grace) was forked away — evict it.
    fn on_finalized(
        &mut self,
        slot: u64,
        repo: &Repo,
        progress: &Arc<ProgressState>,
    ) -> Result<()> {
        self.finalized_seen.insert(slot);
        repo.set_finalized_slot(slot as i64)
            .with_context(|| format!("advancing finalized watermark to {slot}"))?;
        progress.record_finalized(slot);
        metrics::gauge!("solana_indexer_finalized_slot").set(slot as f64);

        let mut evict = Vec::new();
        for &s in self.provisional.range(..=slot) {
            if self.finalized_seen.contains(&s) {
                // Durable — no longer provisional.
                evict.push((s, false));
            } else if slot.saturating_sub(s) > FORK_GRACE_SLOTS {
                evict.push((s, true));
            }
        }
        for (s, forked) in evict {
            self.provisional.remove(&s);
            if forked {
                error!(
                    alert_id = "solana-indexer-fork-evicted",
                    slot = s,
                    watermark = slot,
                    "provisional slot skipped by the finalized chain — evicting its events"
                );
                let deleted = repo
                    .evict_forked_slot(s as i64)
                    .with_context(|| format!("evicting forked slot {s}"))?;
                metrics::counter!("solana_indexer_forked_slots_evicted_total").increment(1);
                warn!(slot = s, deleted, "forked slot evicted");
            }
        }
        self.prune(slot);
        Ok(())
    }

    /// Explicit dead-slot signal: drop anything buffered, evict anything
    /// applied.
    fn on_dead(&mut self, slot: u64, repo: &Repo) -> Result<()> {
        if self.pending.remove(&slot).is_some() {
            warn!(slot, "dead slot had buffered (unapplied) events — dropped");
        }
        if self.provisional.remove(&slot) {
            error!(
                alert_id = "solana-indexer-fork-evicted",
                slot, "dead-slot notification for an applied slot — evicting its events"
            );
            let deleted = repo
                .evict_forked_slot(slot as i64)
                .with_context(|| format!("evicting dead slot {slot}"))?;
            metrics::counter!("solana_indexer_forked_slots_evicted_total").increment(1);
            warn!(slot, deleted, "dead slot evicted");
        }
        Ok(())
    }

    fn prune(&mut self, now: u64) {
        let cutoff = now.saturating_sub(PRUNE_DEPTH);
        self.block_time_ms = self.block_time_ms.split_off(&cutoff);
        self.finalized_seen = self.finalized_seen.split_off(&cutoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Program, Pubkey};

    fn opts() -> IngestOptions {
        IngestOptions {
            endpoint: "https://example".into(),
            api_key: String::new(),
            programs: ProgramSet::new([(Pubkey([1; 32]), Program::Core)]),
            program_ids: vec![Pubkey([1; 32]).to_base58()],
            from_slot: None,
            initial_provisional: vec![],
        }
    }

    #[test]
    fn subscribe_request_carries_all_three_filters_at_confirmed() {
        let mut o = opts();
        o.from_slot = Some(1234);
        let req = build_request(&o);
        assert_eq!(req.commitment, Some(CommitmentLevel::Confirmed as i32));
        assert_eq!(req.from_slot, Some(1234));
        let tx = &req.transactions["protocol"];
        assert_eq!(tx.vote, Some(false));
        assert_eq!(tx.failed, Some(false));
        assert_eq!(tx.account_include, o.program_ids);
        assert_eq!(req.slots["slots"].filter_by_commitment, Some(false));
        assert!(req.blocks_meta.contains_key("meta"));
    }

    #[test]
    fn prune_bounds_the_bookkeeping_maps() {
        let mut st = WorkerState::new(&opts());
        st.block_time_ms.insert(1, 1);
        st.finalized_seen.insert(1);
        st.block_time_ms.insert(50_000, 1);
        st.finalized_seen.insert(50_000);
        st.prune(50_000);
        assert!(!st.block_time_ms.contains_key(&1));
        assert!(!st.finalized_seen.contains(&1));
        assert!(st.block_time_ms.contains_key(&50_000));
    }
}
