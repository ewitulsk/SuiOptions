//! In-memory event log + materialized views.
//!
//! The store is the single source of truth for the off-chain stack: every
//! `ChainEvent` flows through [`Store::ingest`], which assigns a monotonic
//! `sequence`, updates the materialized views (accounts, buckets, positions),
//! appends to the log, and broadcasts the resulting [`IndexedEvent`] to
//! subscribers.
//!
//! The log holds the entire history. `Store::snapshot(after_sequence)` is the
//! catch-up primitive: a new subscriber asks "give me everything after N",
//! then live-streams from the broadcast channel. Channels have a bounded
//! capacity and skip — but the subscriber can recover from the gap by
//! re-snapshotting, so loss is recoverable.

use std::collections::BTreeMap;

use parking_lot::RwLock;
use tokio::sync::broadcast;

use shared::protocol_types::asset::AssetType;
use shared::protocol_types::events::{
    AccountDeposit, AccountWithdraw, BucketCreated, ChainEvent, Exercised, IndexedEvent, Redeemed,
    WriteExecuted,
};
use shared::protocol_types::ids::{ObjectId, SuiAddress};

use crate::db::models::{
    u128_to_bigdecimal, u64_to_bigdecimal, AccountBalanceRow, AccountRow, BucketRow, PositionRow,
};
use crate::db::{CheckpointBatch, EventBuild, HydratedViews};

/// What we keep per Account: balances per asset type, plus the registered
/// signing pubkey (so the quoting service can verify quotes locally as
/// defense-in-depth even while it lazy-loads its own copy).
#[derive(Clone, Debug, Default)]
pub struct AccountState {
    pub owner: Option<SuiAddress>,
    pub signing_pubkey: Vec<u8>,
    pub balances: BTreeMap<AssetType, u64>,
}

/// What we keep per Bucket: cursor state + identity. Strike and asset types
/// are immutable across the bucket's life.
#[derive(Clone, Debug)]
pub struct BucketState {
    pub asset_type: AssetType,
    pub settlement_type: AssetType,
    pub strike: u64,
    pub expiry_ms: u64,
    pub total_written: u128,
    pub exercise_cursor: u128,
    pub cleaned: bool,
}

/// A live position derived from a `WriteExecuted` event. `Redeemed` removes
/// it; pre-expiry it just sits here so the writer's UI can resolve it by id.
#[derive(Clone, Debug)]
pub struct PositionState {
    pub bucket_id: ObjectId,
    pub recipient: SuiAddress,
    pub range_start: u128,
    pub range_end: u128,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub latest_sequence: u64,
    pub events: Vec<IndexedEvent>,
}

/// Output of [`Store::stage_batch`]. The `indexed` events are already in the
/// in-memory log and just await broadcast; `db_batch` is what
/// [`crate::db::Repo::apply_checkpoint`] writes.
pub struct StagedBatch {
    pub indexed: Vec<IndexedEvent>,
    pub db_batch: CheckpointBatch,
}

#[derive(Debug)]
struct Inner {
    log: Vec<IndexedEvent>,
    /// Sequence numbers start at 1. `after_sequence: 0` therefore means
    /// "from before the beginning — give me everything".
    next_sequence: u64,
    accounts: BTreeMap<ObjectId, AccountState>,
    buckets: BTreeMap<ObjectId, BucketState>,
    // Positions are keyed off the WriteExecuted's range_start since the
    // position object id isn't in the event payload (PositionNFTs are minted
    // and transferred to `position_nft_recipient` — we treat the range
    // identity as the off-chain handle until the indexer can resolve real
    // NFT ids).
    positions: BTreeMap<(ObjectId, u128), PositionState>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            log: Vec::new(),
            next_sequence: 1,
            accounts: BTreeMap::new(),
            buckets: BTreeMap::new(),
            positions: BTreeMap::new(),
        }
    }
}

pub struct Store {
    inner: RwLock<Inner>,
    /// Live broadcast of `IndexedEvent`s as they're ingested. Subscribers
    /// that fall behind will see `RecvError::Lagged`; they're expected to
    /// re-snapshot from the log and resume.
    tx: broadcast::Sender<IndexedEvent>,
}

impl Default for Store {
    fn default() -> Self {
        Self::new(1024)
    }
}

impl Store {
    pub fn new(broadcast_capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(broadcast_capacity);
        Self {
            inner: RwLock::new(Inner::default()),
            tx,
        }
    }

    /// Wrap a raw `ChainEvent` with sequence metadata, update materialized
    /// state, append to the log, broadcast.
    pub fn ingest(&self, event: ChainEvent, timestamp_ms: u64) -> IndexedEvent {
        let mut inner = self.inner.write();
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;
        apply_event(&mut inner, &event);
        let indexed = IndexedEvent {
            sequence,
            timestamp_ms,
            event,
        };
        inner.log.push(indexed.clone());
        // It's fine if no subscribers are listening — `send` returns Err but
        // the event is still on the log.
        let _ = self.tx.send(indexed.clone());
        indexed
    }

    /// Stage every event from a checkpoint under a single lock: apply each
    /// to in-memory state, assign sequences, build the [`CheckpointBatch`]
    /// the [`crate::db::Repo`] will persist in one transaction, and append
    /// to the in-memory log. Returns the `IndexedEvent`s in order so the
    /// caller can broadcast them *after* the DB write succeeds — this keeps
    /// the invariant "what's on the wire is durable in Postgres".
    ///
    /// `events` is `(ChainEvent, tx_digest_base58, event_index_within_tx)`.
    pub fn stage_batch(
        &self,
        checkpoint: u64,
        timestamp_ms: u64,
        events: Vec<(ChainEvent, String, i32)>,
    ) -> anyhow::Result<StagedBatch> {
        let mut inner = self.inner.write();

        // Empty checkpoint: still emit a batch so the worker can advance
        // `indexer_progress` and resume past it on restart.
        if events.is_empty() {
            let last_sequence = inner.log.last().map(|e| e.sequence as i64).unwrap_or(0);
            return Ok(StagedBatch {
                indexed: Vec::new(),
                db_batch: CheckpointBatch::empty(checkpoint as i64, last_sequence),
            });
        }

        let mut indexed = Vec::with_capacity(events.len());
        let mut db_batch = CheckpointBatch::empty(checkpoint as i64, 0);

        for (event, tx_digest, event_index) in events {
            let sequence = inner.next_sequence;
            inner.next_sequence += 1;
            apply_event(&mut inner, &event);
            // Snapshot whatever views the event touched into the DB batch.
            stage_event_into_batch(&inner, &event, sequence as i64, &mut db_batch);
            db_batch.events.push(EventBuild::new_event_row(
                sequence as i64,
                checkpoint as i64,
                tx_digest,
                event_index,
                timestamp_ms as i64,
                &event,
            )?);
            let ev = IndexedEvent {
                sequence,
                timestamp_ms,
                event,
            };
            inner.log.push(ev.clone());
            indexed.push(ev);
        }

        db_batch.last_sequence = (inner.next_sequence - 1) as i64;
        Ok(StagedBatch { indexed, db_batch })
    }

    /// Push staged events to the broadcast channel. Called by the worker
    /// after `Repo::apply_checkpoint` returns Ok.
    pub fn broadcast_staged(&self, indexed: &[IndexedEvent]) {
        for ev in indexed {
            // It's fine if no subscribers are listening.
            let _ = self.tx.send(ev.clone());
        }
    }

    /// Replace the in-memory views with the contents of a [`HydratedViews`]
    /// loaded from Postgres at boot. Also bumps `next_sequence` to one past
    /// the highest persisted sequence so newly ingested events stay
    /// monotonic across restarts.
    pub fn hydrate(&self, views: HydratedViews, last_sequence: u64, recent_log: Vec<IndexedEvent>) {
        let mut inner = self.inner.write();
        inner.accounts = views.accounts;
        inner.buckets = views.buckets;
        inner.positions = views.positions;
        inner.log = recent_log;
        inner.next_sequence = last_sequence + 1;
    }

    /// Every event strictly after `after_sequence`. `0` means "from the
    /// beginning"; callers that have caught up should pass the last sequence
    /// they observed.
    pub fn snapshot_after(&self, after_sequence: u64) -> Snapshot {
        let inner = self.inner.read();
        let events: Vec<_> = inner
            .log
            .iter()
            .filter(|e| e.sequence > after_sequence)
            .cloned()
            .collect();
        // `latest_sequence` is the highest published sequence, or
        // `after_sequence` if nothing's been published yet — consumers should
        // use it as their resume cursor.
        let latest_sequence = inner
            .log
            .last()
            .map(|e| e.sequence)
            .unwrap_or(after_sequence);
        Snapshot {
            latest_sequence,
            events,
        }
    }

    pub fn latest_sequence(&self) -> u64 {
        let inner = self.inner.read();
        inner.log.last().map(|e| e.sequence).unwrap_or(0)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<IndexedEvent> {
        self.tx.subscribe()
    }

    pub fn account(&self, id: &ObjectId) -> Option<AccountState> {
        self.inner.read().accounts.get(id).cloned()
    }

    pub fn bucket(&self, id: &ObjectId) -> Option<BucketState> {
        self.inner.read().buckets.get(id).cloned()
    }

    pub fn positions_for_recipient(&self, recipient: &SuiAddress) -> Vec<PositionState> {
        self.inner
            .read()
            .positions
            .values()
            .filter(|p| p.recipient == *recipient)
            .cloned()
            .collect()
    }

    pub fn all_buckets(&self) -> Vec<(ObjectId, BucketState)> {
        self.inner
            .read()
            .buckets
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect()
    }

    pub fn account_count(&self) -> usize {
        self.inner.read().accounts.len()
    }

    pub fn bucket_count(&self) -> usize {
        self.inner.read().buckets.len()
    }

    pub fn position_count(&self) -> usize {
        self.inner.read().positions.len()
    }
}

/// Read the post-apply state of whatever entity `event` touched and push
/// the appropriate rows into `batch`. Called immediately after `apply_event`
/// so `inner` reflects the new values.
fn stage_event_into_batch(
    inner: &Inner,
    event: &ChainEvent,
    sequence: i64,
    batch: &mut CheckpointBatch,
) {
    match event {
        ChainEvent::BucketCreated(b) => {
            if let Some(state) = inner.buckets.get(&b.bucket_id) {
                batch.buckets.push(bucket_row(b.bucket_id, state, sequence));
            }
        }
        ChainEvent::WriteExecuted(w) => {
            if let Some(state) = inner.buckets.get(&w.bucket_id) {
                batch.buckets.push(bucket_row(w.bucket_id, state, sequence));
            }
            if let Some(state) = inner.positions.get(&(w.bucket_id, w.range_start)) {
                batch
                    .position_upserts
                    .push(position_row(state, sequence));
            }
        }
        ChainEvent::Exercised(e) => {
            if let Some(state) = inner.buckets.get(&e.bucket_id) {
                batch.buckets.push(bucket_row(e.bucket_id, state, sequence));
            }
        }
        ChainEvent::Redeemed(r) => {
            // The position was removed by apply_event; tell the repo to delete.
            batch
                .position_deletes
                .push((r.bucket_id.to_hex(), u128_to_bigdecimal(r.range_start)));
        }
        ChainEvent::BucketCleaned(c) => {
            if let Some(state) = inner.buckets.get(&c.bucket_id) {
                batch.buckets.push(bucket_row(c.bucket_id, state, sequence));
            }
        }
        ChainEvent::AccountCreated(a) => {
            if let Some(state) = inner.accounts.get(&a.account_id) {
                batch.accounts.push(account_row(a.account_id, state, sequence));
            }
        }
        ChainEvent::AccountDeposit(d) => {
            if let Some(state) = inner.accounts.get(&d.account_id) {
                // Deposit may also create the row if the account was never
                // seen via AccountCreated (defensive — apply_account_delta
                // calls .entry().or_default()).
                batch
                    .accounts
                    .push(account_row(d.account_id, state, sequence));
                if let Some(bal) = state.balances.get(&d.asset_type) {
                    batch.account_balances.push(balance_row(
                        d.account_id,
                        &d.asset_type,
                        *bal,
                        sequence,
                    ));
                }
            }
        }
        ChainEvent::AccountWithdraw(w) => {
            if let Some(state) = inner.accounts.get(&w.account_id) {
                if let Some(bal) = state.balances.get(&w.asset_type) {
                    batch.account_balances.push(balance_row(
                        w.account_id,
                        &w.asset_type,
                        *bal,
                        sequence,
                    ));
                }
            }
        }
        ChainEvent::SigningKeyRotated(r) => {
            if let Some(state) = inner.accounts.get(&r.account_id) {
                batch
                    .accounts
                    .push(account_row(r.account_id, state, sequence));
            }
        }
        ChainEvent::ExpiredOptionBurned(_)
        | ChainEvent::FeeUpdated(_)
        | ChainEvent::TreasuryWithdrawn(_) => {
            // No materialised-view change. The event itself still lands in
            // `indexed_events` via the caller.
        }
    }
}

fn bucket_row(id: ObjectId, state: &BucketState, sequence: i64) -> BucketRow {
    BucketRow {
        bucket_id: id.to_hex(),
        asset_type: state.asset_type.as_str().to_string(),
        settlement_type: state.settlement_type.as_str().to_string(),
        strike: u64_to_bigdecimal(state.strike),
        expiry_ms: state.expiry_ms as i64,
        total_written: u128_to_bigdecimal(state.total_written),
        exercise_cursor: u128_to_bigdecimal(state.exercise_cursor),
        cleaned: state.cleaned,
        updated_at_seq: sequence,
    }
}

fn position_row(state: &PositionState, sequence: i64) -> PositionRow {
    PositionRow {
        bucket_id: state.bucket_id.to_hex(),
        range_start: u128_to_bigdecimal(state.range_start),
        range_end: u128_to_bigdecimal(state.range_end),
        recipient: state.recipient.to_hex(),
        updated_at_seq: sequence,
    }
}

fn account_row(id: ObjectId, state: &AccountState, sequence: i64) -> AccountRow {
    AccountRow {
        account_id: id.to_hex(),
        owner: state.owner.as_ref().map(|o| o.to_hex()),
        signing_pubkey: state.signing_pubkey.clone(),
        updated_at_seq: sequence,
    }
}

fn balance_row(
    account_id: ObjectId,
    asset_type: &AssetType,
    balance: u64,
    sequence: i64,
) -> AccountBalanceRow {
    AccountBalanceRow {
        account_id: account_id.to_hex(),
        asset_type: asset_type.as_str().to_string(),
        balance: u64_to_bigdecimal(balance),
        updated_at_seq: sequence,
    }
}

fn apply_event(inner: &mut Inner, event: &ChainEvent) {
    match event {
        ChainEvent::BucketCreated(b) => apply_bucket_created(inner, b),
        ChainEvent::WriteExecuted(w) => apply_write_executed(inner, w),
        ChainEvent::Exercised(e) => apply_exercised(inner, e),
        ChainEvent::Redeemed(r) => apply_redeemed(inner, r),
        ChainEvent::ExpiredOptionBurned(_) => {} // no state change
        ChainEvent::BucketCleaned(c) => {
            if let Some(b) = inner.buckets.get_mut(&c.bucket_id) {
                b.cleaned = true;
            }
        }
        ChainEvent::AccountCreated(a) => {
            inner
                .accounts
                .entry(a.account_id)
                .or_insert_with(AccountState::default)
                .signing_pubkey = a.signing_pubkey.clone();
            if let Some(acct) = inner.accounts.get_mut(&a.account_id) {
                acct.owner = Some(a.owner);
            }
        }
        ChainEvent::AccountDeposit(d) => apply_account_delta(inner, d, true),
        ChainEvent::AccountWithdraw(w) => apply_account_delta(inner, w, false),
        ChainEvent::SigningKeyRotated(r) => {
            if let Some(acct) = inner.accounts.get_mut(&r.account_id) {
                acct.signing_pubkey = r.new_pubkey.clone();
            }
        }
        ChainEvent::FeeUpdated(_) | ChainEvent::TreasuryWithdrawn(_) => {}
    }
}

fn apply_bucket_created(inner: &mut Inner, b: &BucketCreated) {
    inner.buckets.insert(
        b.bucket_id,
        BucketState {
            asset_type: b.asset_type.clone(),
            settlement_type: b.settlement_type.clone(),
            strike: b.strike,
            expiry_ms: b.expiry_ms,
            total_written: 0,
            exercise_cursor: 0,
            cleaned: false,
        },
    );
}

fn apply_write_executed(inner: &mut Inner, w: &WriteExecuted) {
    if let Some(b) = inner.buckets.get_mut(&w.bucket_id) {
        b.total_written = w.range_end;
    }
    // The signer's account paid out / collected in their off-chain Account.
    // Premium routing — see §3.3.4. For the materialised view, model it as:
    // the signer is *debited* the side they provide; the executor's side is
    // an off-account wallet transfer that we don't see directly.
    //
    // Writer flow (signer = Trader MM): signer.Settlement -= gross_premium.
    // Trader flow (signer = Writer MM): signer.Underlying -= write_amount.
    //
    // We can't tell the flow apart from a single event in isolation. We do
    // know the bucket's settlement and underlying types from BucketCreated;
    // we can infer the side from whether the signer's pre-event balances
    // suggest one direction or the other. Cleanest is: trust the events
    // about deposits/withdraws to be authoritative for balances — this
    // event only mutates the cursor.
    //
    // Positions: the position NFT goes to `position_nft_recipient`.
    inner.positions.insert(
        (w.bucket_id, w.range_start),
        PositionState {
            bucket_id: w.bucket_id,
            recipient: w.position_nft_recipient,
            range_start: w.range_start,
            range_end: w.range_end,
        },
    );
}

fn apply_exercised(inner: &mut Inner, e: &Exercised) {
    if let Some(b) = inner.buckets.get_mut(&e.bucket_id) {
        b.exercise_cursor = e.cursor_after;
    }
}

fn apply_redeemed(inner: &mut Inner, r: &Redeemed) {
    inner.positions.remove(&(r.bucket_id, r.range_start));
}

fn apply_account_delta<E: AccountDelta>(inner: &mut Inner, e: &E, is_deposit: bool) {
    let acct = inner.accounts.entry(e.account_id()).or_default();
    let bal = acct.balances.entry(e.asset_type().clone()).or_insert(0);
    if is_deposit {
        *bal = bal.saturating_add(e.amount());
    } else {
        *bal = bal.saturating_sub(e.amount());
    }
}

trait AccountDelta {
    fn account_id(&self) -> ObjectId;
    fn asset_type(&self) -> &AssetType;
    fn amount(&self) -> u64;
}

impl AccountDelta for AccountDeposit {
    fn account_id(&self) -> ObjectId {
        self.account_id
    }
    fn asset_type(&self) -> &AssetType {
        &self.asset_type
    }
    fn amount(&self) -> u64 {
        self.amount
    }
}

impl AccountDelta for AccountWithdraw {
    fn account_id(&self) -> ObjectId {
        self.account_id
    }
    fn asset_type(&self) -> &AssetType {
        &self.asset_type
    }
    fn amount(&self) -> u64 {
        self.amount
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::protocol_types::events::{AccountCreated, AccountDeposit, BucketCreated};

    fn bucket_evt(id: u8) -> ChainEvent {
        ChainEvent::BucketCreated(BucketCreated {
            bucket_id: ObjectId::new([id; 32]),
            asset_type: AssetType::new("BTC"),
            settlement_type: AssetType::new("USDC"),
            expiry_ms: 1_000,
            strike: 50_000_000,
        })
    }

    #[test]
    fn ingest_assigns_monotonic_sequence_and_appends_log() {
        let store = Store::default();
        let a = store.ingest(bucket_evt(0x01), 1);
        let b = store.ingest(bucket_evt(0x02), 2);
        // Sequences start at 1 (0 means "from before the beginning").
        assert_eq!(a.sequence, 1);
        assert_eq!(b.sequence, 2);
        // `after_sequence: 0` → everything.
        assert_eq!(store.snapshot_after(0).events.len(), 2);
        // `after_sequence: 1` → only events strictly after seq 1.
        let snap = store.snapshot_after(1);
        assert_eq!(snap.events.len(), 1);
        assert_eq!(snap.events[0].sequence, 2);
    }

    #[test]
    fn bucket_state_tracks_cursor_and_total_written() {
        let store = Store::default();
        let id = ObjectId::new([0x11; 32]);
        store.ingest(
            ChainEvent::BucketCreated(BucketCreated {
                bucket_id: id,
                asset_type: AssetType::new("BTC"),
                settlement_type: AssetType::new("USDC"),
                expiry_ms: 1_000,
                strike: 50,
            }),
            1,
        );
        store.ingest(
            ChainEvent::WriteExecuted(WriteExecuted {
                bucket_id: id,
                signer_account_id: ObjectId::ZERO,
                signer_token_recipient: SuiAddress::ZERO,
                executor: SuiAddress::ZERO,
                position_nft_recipient: SuiAddress::new([0x77; 32]),
                call_token_recipient: SuiAddress::ZERO,
                write_amount: 10,
                gross_premium: 5,
                fee: 0,
                net_premium: 5,
                range_start: 0,
                range_end: 10,
                nonce: 1,
            }),
            2,
        );
        store.ingest(
            ChainEvent::Exercised(Exercised {
                bucket_id: id,
                exerciser: SuiAddress::ZERO,
                amount: 4,
                settlement_paid: 200,
                cursor_after: 4,
            }),
            3,
        );

        let b = store.bucket(&id).unwrap();
        assert_eq!(b.total_written, 10);
        assert_eq!(b.exercise_cursor, 4);

        let writer = SuiAddress::new([0x77; 32]);
        let positions = store.positions_for_recipient(&writer);
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].range_end, 10);
    }

    #[test]
    fn account_balances_apply_deposit_and_withdraw() {
        let store = Store::default();
        let acct = ObjectId::new([0xab; 32]);
        store.ingest(
            ChainEvent::AccountCreated(AccountCreated {
                account_id: acct,
                owner: SuiAddress::new([0xcd; 32]),
                signing_scheme: shared::protocol_types::SigningScheme::Ed25519,
                signing_pubkey: vec![0x01; 32],
            }),
            1,
        );
        store.ingest(
            ChainEvent::AccountDeposit(AccountDeposit {
                account_id: acct,
                asset_type: AssetType::new("USDC"),
                amount: 1_000,
            }),
            2,
        );
        store.ingest(
            ChainEvent::AccountWithdraw(AccountWithdraw {
                account_id: acct,
                asset_type: AssetType::new("USDC"),
                amount: 250,
            }),
            3,
        );
        let s = store.account(&acct).unwrap();
        assert_eq!(s.balances[&AssetType::new("USDC")], 750);
        assert_eq!(s.signing_pubkey, vec![0x01; 32]);
        assert_eq!(s.owner, Some(SuiAddress::new([0xcd; 32])));
    }

    #[tokio::test]
    async fn broadcast_delivers_live_events_to_subscribers() {
        let store = Store::new(8);
        let mut rx = store.subscribe();
        store.ingest(bucket_evt(0x05), 1);
        let got = rx.recv().await.unwrap();
        assert_eq!(got.sequence, 1);
        matches!(got.event, ChainEvent::BucketCreated(_));
    }

    #[test]
    fn snapshot_after_filters_old_events() {
        let store = Store::default();
        for i in 0..5 {
            store.ingest(bucket_evt(i as u8), i);
        }
        // Sequences emitted: 1, 2, 3, 4, 5.
        let snap = store.snapshot_after(3);
        assert_eq!(snap.latest_sequence, 5);
        assert_eq!(snap.events.len(), 2);
        assert_eq!(snap.events[0].sequence, 4);
        assert_eq!(snap.events[1].sequence, 5);
    }
}
