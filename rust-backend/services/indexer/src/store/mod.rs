//! In-memory materialized views.
//!
//! Every `ChainEvent` flows through [`Store::stage_batch`] (or
//! [`Store::ingest`] in tests), which assigns a monotonic `sequence` and
//! updates the materialized views (accounts, buckets, positions). The views
//! are a write-through cache over Postgres: the worker persists each
//! checkpoint via [`crate::db::Repo`], and the cache is rehydrated from
//! Postgres on boot. Consumers read protocol state via the GraphQL query API,
//! never directly from this store.

use std::collections::BTreeMap;

use parking_lot::RwLock;
use tracing::{debug, trace};

use protocol_types::asset::AssetType;
use protocol_types::events::{
    AccountDeposit, AccountWithdraw, BucketCreated, ChainEvent, DeepBookPoolCreated, Exercised,
    IndexedEvent, Redeemed, WriteExecuted,
};
use protocol_types::ids::{ObjectId, SuiAddress};

use crate::db::models::{
    u128_to_bigdecimal, u64_to_bigdecimal, AccountBalanceRow, AccountRow, BucketRow,
    DeepBookPoolRow, EventParticipantRow, PositionRow, RfqBidRow, RfqRow, VaultReceiptRow,
    VaultRoundRow, VaultRow,
};
use crate::db::{CheckpointBatch, EventBuild, HydratedViews};

/// What we keep per Account: balances per asset type, plus the registered
/// signing pubkey (so the quoting service can verify quotes locally as
/// defense-in-depth even while it lazy-loads its own copy).
#[derive(Clone, Debug, Default)]
pub struct AccountState {
    pub owner: Option<SuiAddress>,
    pub signing_pubkey: Vec<u8>,
    /// Registered signing scheme, set from `AccountCreated` /
    /// `SigningKeyRotated`. `None` only for rows hydrated before the scheme
    /// column was backfilled.
    pub signing_scheme: Option<protocol_types::SigningScheme>,
    pub balances: BTreeMap<AssetType, u64>,
}

/// What we keep per Bucket: cursor state + identity. Strike, scale, and
/// asset types are immutable across the bucket's life.
#[derive(Clone, Debug)]
pub struct BucketState {
    pub asset_type: AssetType,
    pub settlement_type: AssetType,
    pub call_type: AssetType,
    pub strike: u128,
    pub strike_scale: u8,
    pub expiry_ms: u64,
    pub total_written: u128,
    pub exercise_cursor: u128,
    pub cleaned: bool,
    pub invalidated: bool,
}

/// A bucket's DeepBook trading venue (SO-152). Written once per bucket —
/// first `PoolCreated` for a call coin wins; later ones are ignored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeepBookPoolState {
    pub pool_id: ObjectId,
    pub base_asset_type: AssetType,
    pub quote_asset_type: AssetType,
    pub tick_size: u64,
    pub lot_size: u64,
    pub min_size: u64,
    pub taker_fee: u64,
    pub maker_fee: u64,
}

/// A live position derived from a `WriteExecuted` event. `Redeemed` removes
/// it; pre-expiry it just sits here so the writer's UI can resolve it by id.
#[derive(Clone, Debug)]
pub struct PositionState {
    pub bucket_id: ObjectId,
    /// On-chain `Position` object id. Captured from
    /// `WriteExecuted.position_id` at mint. The frontend needs this to
    /// build a `redeem_position` PTB. `Position` objects are transferable
    /// via `sui::transfer::public_transfer` so this id is stable across
    /// owners; `recipient` may go stale until transfer-walking lands.
    pub object_id: ObjectId,
    pub recipient: SuiAddress,
    pub range_start: u128,
    pub range_end: u128,
}

/// Lifecycle of an on-chain RFQ auction (C3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RfqStatus {
    Open,
    Settled,
    ExpiredUnsold,
}

impl RfqStatus {
    pub fn tag(self) -> &'static str {
        match self {
            RfqStatus::Open => "open",
            RfqStatus::Settled => "settled",
            RfqStatus::ExpiredUnsold => "expired_unsold",
        }
    }

    pub fn from_str_tag(s: &str) -> anyhow::Result<Self> {
        match s {
            "open" => Ok(RfqStatus::Open),
            "settled" => Ok(RfqStatus::Settled),
            "expired_unsold" => Ok(RfqStatus::ExpiredUnsold),
            other => Err(anyhow::anyhow!("unknown rfq status {other:?}")),
        }
    }
}

/// One RFQ auction's lifecycle, from `RfqCreated` through `RfqBid`s to
/// its terminal `RfqSettled` / `RfqExpiredUnsold` (C3).
#[derive(Clone, Debug, PartialEq)]
pub struct RfqState {
    pub bucket_id: ObjectId,
    pub origin: ObjectId,
    pub amount: u64,
    pub reserve_premium: u64,
    /// Tracks anti-snipe extensions via `RfqBid.new_deadline_ms`.
    pub deadline_ms: u64,
    pub best_premium: Option<u64>,
    pub best_bidder: Option<SuiAddress>,
    pub status: RfqStatus,
    pub winner: Option<SuiAddress>,
    pub net_premium: Option<u64>,
    pub position_id: Option<ObjectId>,
    /// Premium before the protocol RFQ fee; set at settle.
    pub gross_premium: Option<u64>,
    /// Protocol RFQ fee taken at settle (`gross_premium − net_premium`).
    pub fee: Option<u64>,
}

/// One covered-call vault's headline state, assembled from its event
/// stream (D2). Balance-precise fields (deployable etc.) are deliberately
/// not modeled — they need object reads; consumers wanting them ask the
/// chain. What's here is exactly what the events state.
#[derive(Clone, Debug, PartialEq)]
pub struct VaultState {
    pub underlying_type: AssetType,
    pub settlement_type: AssetType,
    pub share_type: AssetType,
    /// Current round: last finalized + 1 (0 = pre-genesis settling).
    pub round: u64,
    pub current_bucket: Option<ObjectId>,
    pub latest_pps: Option<u128>,
    /// Share supply after the last finalize (burn + mint applied).
    pub total_shares: u64,
    /// Deposits queued for the next round.
    pub pending_deposits: u64,
    pub deposits_paused: bool,
    /// Active VaultConfig snapshot (consumer-facing subset), from
    /// VaultCreated (genesis) then VaultConfigApplied (each finalize).
    pub mgmt_fee_bps_annual: Option<u64>,
    pub perf_fee_bps: Option<u64>,
    pub round_ms: Option<u64>,
    pub selling_window_ms: Option<u64>,
    pub min_strike_bps_over_spot: Option<u64>,
    pub max_strike_bps_over_spot: Option<u64>,
}

/// Per-round track record (D2): selection fields land first, fees and
/// the finalize economics fill in as the round progresses.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VaultRoundState {
    pub bucket_id: Option<ObjectId>,
    pub strike: Option<u128>,
    pub strike_scale: Option<u8>,
    pub expiry_ms: Option<u64>,
    pub pps: Option<u128>,
    pub aum: Option<u64>,
    pub shares: Option<u64>,
    pub premium_collected: Option<u64>,
    pub mgmt_fee: Option<u64>,
    pub perf_fee: Option<u64>,
    pub finalized_at_ms: Option<u64>,
}

/// Aggregated deposit/withdraw receipts per (vault, owner, round, kind).
/// The receipt events don't carry object ids, so per-object rows aren't
/// possible; `amount` is queued underlying (deposits) or escrowed shares
/// (withdrawals), `settled` what's been claimed/completed so far.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReceiptState {
    pub amount: u64,
    pub settled: u64,
}

/// Key for the receipts view: (vault, owner hex, round, kind tag).
pub type ReceiptKey = (ObjectId, String, u64, String);

pub const RECEIPT_DEPOSIT: &str = "deposit";
pub const RECEIPT_WITHDRAW: &str = "withdraw";

/// Output of [`Store::stage_batch`]. The `indexed` events carry the assigned
/// sequences (used for logging); `db_batch` is what
/// [`crate::db::Repo::apply_checkpoint`] writes.
pub struct StagedBatch {
    pub indexed: Vec<IndexedEvent>,
    pub db_batch: CheckpointBatch,
}

#[derive(Debug)]
struct Inner {
    /// Next sequence to assign. Starts at 1.
    next_sequence: u64,
    accounts: BTreeMap<ObjectId, AccountState>,
    buckets: BTreeMap<ObjectId, BucketState>,
    // Positions are keyed off the WriteExecuted's range_start since the
    // position object id isn't in the event payload (Position objects are
    // minted and transferred to `position_recipient` — we treat the range
    // identity as the off-chain handle until the indexer can resolve real
    // object ids).
    positions: BTreeMap<(ObjectId, u128), PositionState>,
    /// bucket_id → its DeepBook venue (SO-152).
    deepbook_pools: BTreeMap<ObjectId, DeepBookPoolState>,
    /// rfq_id → auction lifecycle (C3).
    rfqs: BTreeMap<ObjectId, RfqState>,
    /// vault_id → headline state (D2).
    vaults: BTreeMap<ObjectId, VaultState>,
    /// (vault_id, round) → track record (D2).
    vault_rounds: BTreeMap<(ObjectId, u64), VaultRoundState>,
    /// (vault, owner, round, kind) → receipt aggregate (D2).
    vault_receipts: BTreeMap<ReceiptKey, ReceiptState>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            next_sequence: 1,
            accounts: BTreeMap::new(),
            buckets: BTreeMap::new(),
            positions: BTreeMap::new(),
            deepbook_pools: BTreeMap::new(),
            rfqs: BTreeMap::new(),
            vaults: BTreeMap::new(),
            vault_rounds: BTreeMap::new(),
            vault_receipts: BTreeMap::new(),
        }
    }
}

#[derive(Default)]
pub struct Store {
    inner: RwLock<Inner>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wrap a raw `ChainEvent` with sequence metadata and update materialized
    /// state. Test helper — production ingestion goes through
    /// [`stage_batch`](Self::stage_batch).
    pub fn ingest(&self, event: ChainEvent, timestamp_ms: u64) -> IndexedEvent {
        let mut inner = self.inner.write();
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;
        trace!(sequence, timestamp_ms, event_type = ?std::mem::discriminant(&event), "ingesting event");
        apply_event(&mut inner, &event, timestamp_ms);
        IndexedEvent {
            sequence,
            timestamp_ms,
            event,
        }
    }

    /// Stage every event from a checkpoint under a single lock: apply each
    /// to in-memory state, assign sequences, and build the [`CheckpointBatch`]
    /// the [`crate::db::Repo`] will persist in one transaction. Returns the
    /// `IndexedEvent`s in order for logging.
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
            trace!(checkpoint, "empty checkpoint; advancing progress only");
            let last_sequence = (inner.next_sequence - 1) as i64;
            return Ok(StagedBatch {
                indexed: Vec::new(),
                db_batch: CheckpointBatch::empty(checkpoint as i64, last_sequence),
            });
        }
        debug!(checkpoint, event_count = events.len(), "staging checkpoint batch");

        let mut indexed = Vec::with_capacity(events.len());
        let mut db_batch = CheckpointBatch::empty(checkpoint as i64, 0);

        for (event, tx_digest, event_index) in events {
            let sequence = inner.next_sequence;
            inner.next_sequence += 1;
            apply_event(&mut inner, &event, timestamp_ms);
            // Snapshot whatever views the event touched into the DB batch.
            // tx_digest + timestamp are needed to denormalize position
            // provenance (SO-97), so pass them through before `tx_digest`
            // is moved into the event row below.
            stage_event_into_batch(
                &inner,
                &event,
                sequence as i64,
                &tx_digest,
                timestamp_ms as i64,
                &mut db_batch,
            );
            // Per-event participant edges for the `events(participant:)` filter.
            collect_participants(&inner, &event, sequence as i64, &mut db_batch);
            db_batch.events.push(EventBuild::new_event_row(
                sequence as i64,
                checkpoint as i64,
                tx_digest,
                event_index,
                timestamp_ms as i64,
                &event,
            )?);
            indexed.push(IndexedEvent {
                sequence,
                timestamp_ms,
                event,
            });
        }

        db_batch.last_sequence = (inner.next_sequence - 1) as i64;
        Ok(StagedBatch { indexed, db_batch })
    }

    /// Replace the in-memory views with the contents of a [`HydratedViews`]
    /// loaded from Postgres at boot. Also bumps `next_sequence` to one past
    /// the highest persisted sequence so newly ingested events stay
    /// monotonic across restarts.
    pub fn hydrate(&self, views: HydratedViews, last_sequence: u64) {
        let mut inner = self.inner.write();
        debug!(
            accounts = views.accounts.len(),
            buckets = views.buckets.len(),
            positions = views.positions.len(),
            last_sequence,
            "hydrating store from postgres"
        );
        inner.accounts = views.accounts;
        inner.buckets = views.buckets;
        inner.positions = views.positions;
        inner.deepbook_pools = views.deepbook_pools;
        inner.rfqs = views.rfqs;
        inner.vaults = views.vaults;
        inner.vault_rounds = views.vault_rounds;
        inner.vault_receipts = views.vault_receipts;
        inner.next_sequence = last_sequence + 1;
    }

    pub fn latest_sequence(&self) -> u64 {
        self.inner.read().next_sequence.saturating_sub(1)
    }

    pub fn account(&self, id: &ObjectId) -> Option<AccountState> {
        self.inner.read().accounts.get(id).cloned()
    }

    pub fn bucket(&self, id: &ObjectId) -> Option<BucketState> {
        self.inner.read().buckets.get(id).cloned()
    }

    /// Resolve a bucket by its call coin type (SO-152: DeepBook pools are
    /// tied to buckets via `PoolCreated`'s base asset). Linear scan — the
    /// bucket set is small (a handful per active series). Compares canonical
    /// forms: stored call_types are chain `TypeName`s (no `0x`), while a
    /// pool's base asset arrives `0x`-prefixed from the event type string
    /// (SO-163).
    pub fn bucket_by_call_type(&self, call_type: &AssetType) -> Option<(ObjectId, BucketState)> {
        let target = call_type.to_canonical();
        self.inner
            .read()
            .buckets
            .iter()
            .find(|(_, b)| b.call_type.to_canonical() == target)
            .map(|(id, b)| (*id, b.clone()))
    }

    /// The bucket's DeepBook venue, if one has been created (SO-152).
    pub fn deepbook_pool(&self, bucket_id: &ObjectId) -> Option<DeepBookPoolState> {
        self.inner.read().deepbook_pools.get(bucket_id).cloned()
    }

    /// The bucket whose DeepBook venue is `pool_id` (SO-209). Linear scan over
    /// the small pool set, mirroring [`bucket_by_call_type`](Self::bucket_by_call_type).
    pub fn bucket_by_pool_id(&self, pool_id: &ObjectId) -> Option<ObjectId> {
        self.inner
            .read()
            .deepbook_pools
            .iter()
            .find(|(_, p)| p.pool_id == *pool_id)
            .map(|(bucket_id, _)| *bucket_id)
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

    /// One auction's lifecycle state (C3).
    pub fn rfq(&self, id: &ObjectId) -> Option<RfqState> {
        self.inner.read().rfqs.get(id).cloned()
    }

    /// One vault's headline state (D2).
    pub fn vault(&self, id: &ObjectId) -> Option<VaultState> {
        self.inner.read().vaults.get(id).cloned()
    }

    /// One vault round's track-record entry (D2).
    pub fn vault_round(&self, vault: &ObjectId, round: u64) -> Option<VaultRoundState> {
        self.inner.read().vault_rounds.get(&(*vault, round)).cloned()
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

fn account_owner_hex(inner: &Inner, account_id: &ObjectId) -> Option<String> {
    inner
        .accounts
        .get(account_id)
        .and_then(|a| a.owner)
        .map(|o| o.to_hex())
}

/// Fan an event out to the addresses it touches, each tagged with a role, so
/// the generalized `events(participant:)` query can match "involves address X
/// in ANY role" with a single indexed lookup. Account-scoped events resolve
/// the owner wallet via the in-memory accounts view.
fn collect_participants(
    inner: &Inner,
    event: &ChainEvent,
    sequence: i64,
    batch: &mut CheckpointBatch,
) {
    let mut push = |address: String, role: &str| {
        batch.event_participants.push(EventParticipantRow {
            sequence,
            address,
            role: role.to_string(),
        });
    };
    match event {
        ChainEvent::WriteExecuted(w) => {
            push(w.position_recipient.to_hex(), "position_recipient");
            push(w.call_token_recipient.to_hex(), "call_token_recipient");
            push(w.signer_token_recipient.to_hex(), "signer_token_recipient");
            push(w.executor.to_hex(), "executor");
            if let Some(o) = account_owner_hex(inner, &w.signer_account_id) {
                push(o, "signer_account_owner");
            }
        }
        ChainEvent::Exercised(e) => push(e.exerciser.to_hex(), "exerciser"),
        ChainEvent::Redeemed(r) => push(r.redeemer.to_hex(), "redeemer"),
        ChainEvent::ExpiredOptionBurned(b) => push(b.burner.to_hex(), "burner"),
        ChainEvent::BucketInvalidated(i) => push(i.admin.to_hex(), "admin"),
        ChainEvent::BucketRevalidated(r) => push(r.admin.to_hex(), "admin"),
        ChainEvent::AccountCreated(a) => push(a.owner.to_hex(), "account_owner"),
        ChainEvent::AccountDeposit(d) => {
            if let Some(o) = account_owner_hex(inner, &d.account_id) {
                push(o, "account_owner");
            }
        }
        ChainEvent::AccountWithdraw(w) => {
            if let Some(o) = account_owner_hex(inner, &w.account_id) {
                push(o, "account_owner");
            }
        }
        ChainEvent::SigningKeyRotated(r) => {
            if let Some(o) = account_owner_hex(inner, &r.account_id) {
                push(o, "account_owner");
            }
        }
        ChainEvent::TreasuryWithdrawn(t) => push(t.recipient.to_hex(), "treasury_recipient"),
        ChainEvent::CollateralizedWrite(w) => push(w.writer.to_hex(), "writer"),
        ChainEvent::RfqBid(b) => {
            push(b.bidder.to_hex(), "bidder");
            push(b.call_recipient.to_hex(), "call_recipient");
        }
        ChainEvent::RfqSettled(s) => {
            push(s.winner.to_hex(), "winner");
            push(s.call_recipient.to_hex(), "call_recipient");
            push(s.position_recipient.to_hex(), "position_recipient");
        }
        ChainEvent::VaultDeposit(d) => push(d.depositor.to_hex(), "depositor"),
        ChainEvent::SharesClaimed(c) => push(c.owner.to_hex(), "owner"),
        ChainEvent::WithdrawInitiated(w) => push(w.owner.to_hex(), "owner"),
        ChainEvent::WithdrawCompleted(w) => push(w.owner.to_hex(), "owner"),
        ChainEvent::InstantWithdraw(w) => push(w.owner.to_hex(), "owner"),
        ChainEvent::SwapRfqBid(b) => push(b.bidder.to_hex(), "bidder"),
        ChainEvent::SwapRfqSettled(s) => push(s.winner.to_hex(), "winner"),
        // Fills are attributed by BalanceManager id, not wallet — the api-service
        // maps a wallet's BM back to it for cost-basis (SO-209). Both sides are
        // fanned out so a buy or a sell by either party is matchable.
        ChainEvent::DeepBookOrderFilled(f) => {
            push(f.taker_balance_manager_id.to_hex(), "deepbook_taker_bm");
            push(f.maker_balance_manager_id.to_hex(), "deepbook_maker_bm");
        }
        // DeepBookPoolCreated carries no addresses (the creator isn't in the
        // event payload).
        ChainEvent::BucketCreated(_)
        | ChainEvent::BucketCleaned(_)
        | ChainEvent::FeeUpdated(_)
        | ChainEvent::DeepBookPoolCreated(_)
        | ChainEvent::RfqCreated(_)
        | ChainEvent::RfqExpiredUnsold(_)
        | ChainEvent::SwapRfqCreated(_)
        | ChainEvent::SwapRfqUnfilled(_)
        | ChainEvent::VaultCreated(_)
        | ChainEvent::VaultBucketSelected(_)
        | ChainEvent::VaultPositionRedeemed(_)
        | ChainEvent::VaultFeesCharged(_)
        | ChainEvent::VaultRoundFinalized(_)
        | ChainEvent::VaultConfigUpdated(_)
        | ChainEvent::VaultConfigApplied(_)
        | ChainEvent::VaultDepositsPaused(_) => {}
    }
}

/// Read the post-apply state of whatever entity `event` touched and push
/// the appropriate rows into `batch`. Called immediately after `apply_event`
/// so `inner` reflects the new values.
fn stage_event_into_batch(
    inner: &Inner,
    event: &ChainEvent,
    sequence: i64,
    tx_digest: &str,
    timestamp_ms: i64,
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
            // Build the position row straight from the event so we can
            // denormalize provenance (premium / MM / tx / minted-at). `w`
            // carries the structural fields too, so the post-apply state
            // lookup isn't needed.
            batch
                .position_upserts
                .push(write_position_row(w, sequence, tx_digest, timestamp_ms));
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
        ChainEvent::BucketInvalidated(i) => {
            if let Some(state) = inner.buckets.get(&i.bucket_id) {
                batch.buckets.push(bucket_row(i.bucket_id, state, sequence));
            }
        }
        ChainEvent::BucketRevalidated(r) => {
            if let Some(state) = inner.buckets.get(&r.bucket_id) {
                batch.buckets.push(bucket_row(r.bucket_id, state, sequence));
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
        ChainEvent::DeepBookPoolCreated(p) => {
            // First pool wins — stage the row only when this event's pool is
            // the one apply_event recorded (the DB insert is additionally
            // ON CONFLICT DO NOTHING, so replays stay idempotent).
            if inner.deepbook_pools.get(&p.bucket_id).map(|s| s.pool_id) == Some(p.pool_id) {
                batch.deepbook_pools.push(deepbook_pool_row(
                    p,
                    batch.checkpoint,
                    timestamp_ms,
                    sequence,
                ));
            }
        }
        ChainEvent::CollateralizedWrite(w) => {
            // A venue write moved the bucket's cursor (RfqSettled carries
            // its own CollateralizedWrite alongside, emitted by the shared
            // write core, so the bucket row refresh happens here for both).
            if let Some(state) = inner.buckets.get(&w.bucket_id) {
                batch.buckets.push(bucket_row(w.bucket_id, state, sequence));
            }
        }
        // ── RFQ lifecycle (C3): snapshot the post-apply auction state ──
        ChainEvent::RfqCreated(r) => {
            if let Some(state) = inner.rfqs.get(&r.rfq_id) {
                batch.rfqs.push(rfq_row(r.rfq_id, state, sequence));
            }
        }
        ChainEvent::RfqBid(b) => {
            if let Some(state) = inner.rfqs.get(&b.rfq_id) {
                batch.rfqs.push(rfq_row(b.rfq_id, state, sequence));
            }
            batch.rfq_bids.push(RfqBidRow {
                rfq_id: b.rfq_id.to_hex(),
                sequence,
                bidder: b.bidder.to_hex(),
                call_recipient: b.call_recipient.to_hex(),
                premium: u64_to_bigdecimal(b.premium),
            });
        }
        ChainEvent::RfqSettled(s) => {
            if let Some(state) = inner.rfqs.get(&s.rfq_id) {
                batch.rfqs.push(rfq_row(s.rfq_id, state, sequence));
            }
        }
        ChainEvent::RfqExpiredUnsold(e) => {
            if let Some(state) = inner.rfqs.get(&e.rfq_id) {
                batch.rfqs.push(rfq_row(e.rfq_id, state, sequence));
            }
        }
        // ── vault lifecycle (D2) ────────────────────────────────────
        ChainEvent::VaultCreated(v) => {
            if let Some(state) = inner.vaults.get(&v.vault_id) {
                batch.vaults.push(vault_row(v.vault_id, state, sequence));
            }
        }
        ChainEvent::VaultDeposit(d) => {
            stage_vault(inner, d.vault_id, sequence, batch);
            stage_receipt(inner, (d.vault_id, d.depositor.to_hex(), d.round, RECEIPT_DEPOSIT.into()), sequence, batch);
        }
        ChainEvent::InstantWithdraw(w) => {
            stage_vault(inner, w.vault_id, sequence, batch);
            stage_receipt(inner, (w.vault_id, w.owner.to_hex(), w.round, RECEIPT_DEPOSIT.into()), sequence, batch);
        }
        ChainEvent::SharesClaimed(c) => {
            stage_receipt(inner, (c.vault_id, c.owner.to_hex(), c.round, RECEIPT_DEPOSIT.into()), sequence, batch);
        }
        ChainEvent::WithdrawInitiated(w) => {
            stage_receipt(inner, (w.vault_id, w.owner.to_hex(), w.round, RECEIPT_WITHDRAW.into()), sequence, batch);
        }
        ChainEvent::WithdrawCompleted(w) => {
            stage_receipt(inner, (w.vault_id, w.owner.to_hex(), w.round, RECEIPT_WITHDRAW.into()), sequence, batch);
        }
        ChainEvent::VaultBucketSelected(s) => {
            stage_vault(inner, s.vault_id, sequence, batch);
            stage_vault_round(inner, s.vault_id, s.round, sequence, batch);
        }
        ChainEvent::VaultFeesCharged(f) => {
            stage_vault_round(inner, f.vault_id, f.round, sequence, batch);
        }
        ChainEvent::VaultRoundFinalized(f) => {
            stage_vault(inner, f.vault_id, sequence, batch);
            stage_vault_round(inner, f.vault_id, f.round, sequence, batch);
        }
        ChainEvent::VaultDepositsPaused(p) => {
            stage_vault(inner, p.vault_id, sequence, batch);
        }
        ChainEvent::VaultConfigApplied(c) => {
            stage_vault(inner, c.vault_id, sequence, batch);
        }
        ChainEvent::ExpiredOptionBurned(_)
        | ChainEvent::FeeUpdated(_)
        | ChainEvent::TreasuryWithdrawn(_)
        | ChainEvent::VaultPositionRedeemed(_)
        | ChainEvent::SwapRfqCreated(_)
        | ChainEvent::SwapRfqBid(_)
        | ChainEvent::SwapRfqSettled(_)
        | ChainEvent::SwapRfqUnfilled(_)
        | ChainEvent::DeepBookOrderFilled(_)
        | ChainEvent::VaultConfigUpdated(_) => {
            // Log-only events: no materialised view to refresh (the swap
            // auction's effect on the vault is picked up at finalize, as
            // the old VaultProceedsSwapped path was). DeepBook fills live only
            // in the event log + participants (SO-209), read on demand.
        }
    }
}

fn stage_vault(inner: &Inner, vault_id: ObjectId, sequence: i64, batch: &mut CheckpointBatch) {
    if let Some(state) = inner.vaults.get(&vault_id) {
        batch.vaults.push(vault_row(vault_id, state, sequence));
    }
}

fn stage_vault_round(
    inner: &Inner,
    vault_id: ObjectId,
    round: u64,
    sequence: i64,
    batch: &mut CheckpointBatch,
) {
    if let Some(state) = inner.vault_rounds.get(&(vault_id, round)) {
        batch
            .vault_rounds
            .push(vault_round_row(vault_id, round, state, sequence));
    }
}

fn stage_receipt(inner: &Inner, key: ReceiptKey, sequence: i64, batch: &mut CheckpointBatch) {
    if let Some(state) = inner.vault_receipts.get(&key) {
        batch.vault_receipts.push(receipt_row(&key, state, sequence));
    }
}

fn bucket_row(id: ObjectId, state: &BucketState, sequence: i64) -> BucketRow {
    BucketRow {
        bucket_id: id.to_hex(),
        asset_type: state.asset_type.as_str().to_string(),
        settlement_type: state.settlement_type.as_str().to_string(),
        call_type: state.call_type.as_str().to_string(),
        strike: u128_to_bigdecimal(state.strike),
        strike_scale: state.strike_scale as i16,
        expiry_ms: state.expiry_ms as i64,
        total_written: u128_to_bigdecimal(state.total_written),
        exercise_cursor: u128_to_bigdecimal(state.exercise_cursor),
        cleaned: state.cleaned,
        invalidated: state.invalidated,
        updated_at_seq: sequence,
    }
}

fn write_position_row(
    w: &WriteExecuted,
    sequence: i64,
    tx_digest: &str,
    minted_at_ms: i64,
) -> PositionRow {
    PositionRow {
        bucket_id: w.bucket_id.to_hex(),
        range_start: u128_to_bigdecimal(w.range_start),
        range_end: u128_to_bigdecimal(w.range_end),
        object_id: w.position_id.to_hex(),
        recipient: w.position_recipient.to_hex(),
        updated_at_seq: sequence,
        // SO-97 provenance: gross premium the writer received, the
        // counterparty MM account, and the minting tx for explorer links.
        premium_received: u64_to_bigdecimal(w.gross_premium),
        mm_account_id: w.signer_account_id.to_hex(),
        tx_digest: tx_digest.to_string(),
        minted_at_ms,
    }
}

fn deepbook_pool_row(
    p: &DeepBookPoolCreated,
    checkpoint: i64,
    timestamp_ms: i64,
    sequence: i64,
) -> DeepBookPoolRow {
    DeepBookPoolRow {
        bucket_id: p.bucket_id.to_hex(),
        pool_id: p.pool_id.to_hex(),
        base_asset_type: p.base_asset_type.as_str().to_string(),
        quote_asset_type: p.quote_asset_type.as_str().to_string(),
        tick_size: p.tick_size as i64,
        lot_size: p.lot_size as i64,
        min_size: p.min_size as i64,
        taker_fee: p.taker_fee as i64,
        maker_fee: p.maker_fee as i64,
        created_checkpoint: checkpoint,
        created_timestamp_ms: timestamp_ms,
        updated_at_seq: sequence,
    }
}

fn rfq_row(id: ObjectId, state: &RfqState, sequence: i64) -> RfqRow {
    RfqRow {
        rfq_id: id.to_hex(),
        bucket_id: state.bucket_id.to_hex(),
        origin: state.origin.to_hex(),
        amount: u64_to_bigdecimal(state.amount),
        reserve_premium: u64_to_bigdecimal(state.reserve_premium),
        deadline_ms: state.deadline_ms as i64,
        best_premium: state.best_premium.map(u64_to_bigdecimal),
        best_bidder: state.best_bidder.map(|a| a.to_hex()),
        status: state.status.tag().to_string(),
        winner: state.winner.map(|a| a.to_hex()),
        net_premium: state.net_premium.map(u64_to_bigdecimal),
        position_id: state.position_id.map(|p| p.to_hex()),
        gross_premium: state.gross_premium.map(u64_to_bigdecimal),
        fee: state.fee.map(u64_to_bigdecimal),
        updated_at_seq: sequence,
    }
}

fn vault_row(id: ObjectId, state: &VaultState, sequence: i64) -> VaultRow {
    VaultRow {
        vault_id: id.to_hex(),
        underlying_type: state.underlying_type.as_str().to_string(),
        settlement_type: state.settlement_type.as_str().to_string(),
        share_type: state.share_type.as_str().to_string(),
        round: state.round as i64,
        current_bucket: state.current_bucket.map(|b| b.to_hex()),
        latest_pps: state.latest_pps.map(u128_to_bigdecimal),
        total_shares: u64_to_bigdecimal(state.total_shares),
        pending_deposits: u64_to_bigdecimal(state.pending_deposits),
        deposits_paused: state.deposits_paused,
        updated_at_seq: sequence,
        mgmt_fee_bps_annual: state.mgmt_fee_bps_annual.map(|x| x as i64),
        perf_fee_bps: state.perf_fee_bps.map(|x| x as i64),
        round_ms: state.round_ms.map(|x| x as i64),
        selling_window_ms: state.selling_window_ms.map(|x| x as i64),
        min_strike_bps_over_spot: state.min_strike_bps_over_spot.map(|x| x as i64),
        max_strike_bps_over_spot: state.max_strike_bps_over_spot.map(|x| x as i64),
    }
}

fn vault_round_row(
    vault_id: ObjectId,
    round: u64,
    state: &VaultRoundState,
    sequence: i64,
) -> VaultRoundRow {
    VaultRoundRow {
        vault_id: vault_id.to_hex(),
        round: round as i64,
        bucket_id: state.bucket_id.map(|b| b.to_hex()),
        strike: state.strike.map(u128_to_bigdecimal),
        strike_scale: state.strike_scale.map(|s| s as i16),
        expiry_ms: state.expiry_ms.map(|v| v as i64),
        pps: state.pps.map(u128_to_bigdecimal),
        aum: state.aum.map(u64_to_bigdecimal),
        shares: state.shares.map(u64_to_bigdecimal),
        premium_collected: state.premium_collected.map(u64_to_bigdecimal),
        mgmt_fee: state.mgmt_fee.map(u64_to_bigdecimal),
        perf_fee: state.perf_fee.map(u64_to_bigdecimal),
        finalized_at_ms: state.finalized_at_ms.map(|v| v as i64),
        updated_at_seq: sequence,
    }
}

fn receipt_row(key: &ReceiptKey, state: &ReceiptState, sequence: i64) -> VaultReceiptRow {
    VaultReceiptRow {
        vault_id: key.0.to_hex(),
        owner: key.1.clone(),
        round: key.2 as i64,
        kind: key.3.clone(),
        amount: u64_to_bigdecimal(state.amount),
        settled: u64_to_bigdecimal(state.settled),
        updated_at_seq: sequence,
    }
}

fn account_row(id: ObjectId, state: &AccountState, sequence: i64) -> AccountRow {
    AccountRow {
        account_id: id.to_hex(),
        owner: state.owner.as_ref().map(|o| o.to_hex()),
        signing_pubkey: state.signing_pubkey.clone(),
        signing_scheme: state.signing_scheme.map(|s| s.as_u8() as i16),
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

fn apply_event(inner: &mut Inner, event: &ChainEvent, timestamp_ms: u64) {
    match event {
        ChainEvent::BucketCreated(b) => apply_bucket_created(inner, b),
        ChainEvent::WriteExecuted(w) => apply_write_executed(inner, w),
        ChainEvent::Exercised(e) => apply_exercised(inner, e),
        ChainEvent::Redeemed(r) => apply_redeemed(inner, r),
        ChainEvent::ExpiredOptionBurned(_) => {} // no state change
        ChainEvent::DeepBookOrderFilled(_) => {} // log-only (SO-209)
        ChainEvent::BucketCleaned(c) => {
            if let Some(b) = inner.buckets.get_mut(&c.bucket_id) {
                b.cleaned = true;
            }
        }
        ChainEvent::BucketInvalidated(i) => {
            if let Some(b) = inner.buckets.get_mut(&i.bucket_id) {
                b.invalidated = true;
            }
        }
        ChainEvent::BucketRevalidated(r) => {
            if let Some(b) = inner.buckets.get_mut(&r.bucket_id) {
                b.invalidated = false;
            }
        }
        ChainEvent::AccountCreated(a) => {
            let acct = inner
                .accounts
                .entry(a.account_id)
                .or_insert_with(AccountState::default);
            acct.signing_pubkey = a.signing_pubkey.clone();
            acct.signing_scheme = Some(a.signing_scheme);
            acct.owner = Some(a.owner);
        }
        ChainEvent::AccountDeposit(d) => apply_account_delta(inner, d, true),
        ChainEvent::AccountWithdraw(w) => apply_account_delta(inner, w, false),
        ChainEvent::SigningKeyRotated(r) => {
            if let Some(acct) = inner.accounts.get_mut(&r.account_id) {
                acct.signing_pubkey = r.new_pubkey.clone();
                acct.signing_scheme = Some(r.new_scheme);
            }
        }
        ChainEvent::DeepBookPoolCreated(p) => {
            // First pool wins. A second PoolCreated for the same bucket means
            // someone burned 500 DEEP on a duplicate venue — keep the
            // original mapping and surface the anomaly.
            if let Some(existing) = inner.deepbook_pools.get(&p.bucket_id) {
                if existing.pool_id != p.pool_id {
                    tracing::warn!(
                        bucket = %p.bucket_id.to_hex(),
                        kept = %existing.pool_id.to_hex(),
                        ignored = %p.pool_id.to_hex(),
                        "duplicate DeepBook pool for bucket; keeping first"
                    );
                }
            } else {
                inner.deepbook_pools.insert(
                    p.bucket_id,
                    DeepBookPoolState {
                        pool_id: p.pool_id,
                        base_asset_type: p.base_asset_type.clone(),
                        quote_asset_type: p.quote_asset_type.clone(),
                        tick_size: p.tick_size,
                        lot_size: p.lot_size,
                        min_size: p.min_size,
                        taker_fee: p.taker_fee,
                        maker_fee: p.maker_fee,
                    },
                );
            }
        }
        ChainEvent::CollateralizedWrite(w) => {
            // Venue writes (RFQ settles, self-writes) advance the bucket
            // cursor exactly like signed-quote writes.
            if let Some(b) = inner.buckets.get_mut(&w.bucket_id) {
                b.total_written = w.range_end;
            }
        }
        // ── RFQ lifecycle (C3) ──────────────────────────────────────
        ChainEvent::RfqCreated(r) => {
            inner.rfqs.insert(
                r.rfq_id,
                RfqState {
                    bucket_id: r.bucket_id,
                    origin: r.origin,
                    amount: r.amount,
                    reserve_premium: r.reserve_premium,
                    deadline_ms: r.deadline_ms,
                    best_premium: None,
                    best_bidder: None,
                    status: RfqStatus::Open,
                    winner: None,
                    net_premium: None,
                    position_id: None,
                    gross_premium: None,
                    fee: None,
                },
            );
        }
        ChainEvent::RfqBid(b) => {
            if let Some(rfq) = inner.rfqs.get_mut(&b.rfq_id) {
                rfq.best_premium = Some(b.premium);
                rfq.best_bidder = Some(b.bidder);
                rfq.deadline_ms = b.new_deadline_ms;
            }
        }
        ChainEvent::RfqSettled(s) => {
            if let Some(rfq) = inner.rfqs.get_mut(&s.rfq_id) {
                rfq.status = RfqStatus::Settled;
                rfq.winner = Some(s.winner);
                rfq.net_premium = Some(s.net_premium);
                rfq.position_id = Some(s.position_id);
                rfq.gross_premium = Some(s.gross_premium);
                rfq.fee = Some(s.fee);
            }
        }
        ChainEvent::RfqExpiredUnsold(e) => {
            if let Some(rfq) = inner.rfqs.get_mut(&e.rfq_id) {
                rfq.status = RfqStatus::ExpiredUnsold;
            }
        }
        // ── vault lifecycle (D2) ────────────────────────────────────
        ChainEvent::VaultCreated(v) => {
            inner.vaults.insert(
                v.vault_id,
                VaultState {
                    underlying_type: v.underlying_type.clone(),
                    settlement_type: v.settlement_type.clone(),
                    share_type: v.share_type.clone(),
                    round: 0,
                    current_bucket: None,
                    latest_pps: None,
                    total_shares: 0,
                    pending_deposits: 0,
                    deposits_paused: false,
                    mgmt_fee_bps_annual: Some(v.mgmt_fee_bps_annual),
                    perf_fee_bps: Some(v.perf_fee_bps),
                    round_ms: Some(v.round_ms),
                    selling_window_ms: Some(v.selling_window_ms),
                    min_strike_bps_over_spot: Some(v.min_strike_bps_over_spot),
                    max_strike_bps_over_spot: Some(v.max_strike_bps_over_spot),
                },
            );
        }
        ChainEvent::VaultConfigApplied(c) => {
            if let Some(v) = inner.vaults.get_mut(&c.vault_id) {
                v.mgmt_fee_bps_annual = Some(c.mgmt_fee_bps_annual);
                v.perf_fee_bps = Some(c.perf_fee_bps);
                v.round_ms = Some(c.round_ms);
                v.selling_window_ms = Some(c.selling_window_ms);
                v.min_strike_bps_over_spot = Some(c.min_strike_bps_over_spot);
                v.max_strike_bps_over_spot = Some(c.max_strike_bps_over_spot);
            }
        }
        ChainEvent::VaultDeposit(d) => {
            if let Some(v) = inner.vaults.get_mut(&d.vault_id) {
                v.pending_deposits = v.pending_deposits.saturating_add(d.amount);
            }
            let r = inner
                .vault_receipts
                .entry((d.vault_id, d.depositor.to_hex(), d.round, RECEIPT_DEPOSIT.into()))
                .or_default();
            r.amount = r.amount.saturating_add(d.amount);
        }
        ChainEvent::InstantWithdraw(w) => {
            if let Some(v) = inner.vaults.get_mut(&w.vault_id) {
                v.pending_deposits = v.pending_deposits.saturating_sub(w.amount);
            }
            let r = inner
                .vault_receipts
                .entry((w.vault_id, w.owner.to_hex(), w.round, RECEIPT_DEPOSIT.into()))
                .or_default();
            r.settled = r.settled.saturating_add(w.amount);
        }
        ChainEvent::SharesClaimed(c) => {
            let r = inner
                .vault_receipts
                .entry((c.vault_id, c.owner.to_hex(), c.round, RECEIPT_DEPOSIT.into()))
                .or_default();
            r.settled = r.settled.saturating_add(c.amount);
        }
        ChainEvent::WithdrawInitiated(w) => {
            let r = inner
                .vault_receipts
                .entry((w.vault_id, w.owner.to_hex(), w.round, RECEIPT_WITHDRAW.into()))
                .or_default();
            r.amount = r.amount.saturating_add(w.shares);
        }
        ChainEvent::WithdrawCompleted(w) => {
            let r = inner
                .vault_receipts
                .entry((w.vault_id, w.owner.to_hex(), w.round, RECEIPT_WITHDRAW.into()))
                .or_default();
            r.settled = r.settled.saturating_add(w.shares);
        }
        ChainEvent::VaultBucketSelected(s) => {
            if let Some(v) = inner.vaults.get_mut(&s.vault_id) {
                v.current_bucket = Some(s.bucket_id);
                v.round = s.round;
            }
            let r = inner.vault_rounds.entry((s.vault_id, s.round)).or_default();
            r.bucket_id = Some(s.bucket_id);
            r.strike = Some(s.strike);
            r.strike_scale = Some(s.strike_scale);
            r.expiry_ms = Some(s.expiry_ms);
        }
        ChainEvent::VaultFeesCharged(f) => {
            let r = inner.vault_rounds.entry((f.vault_id, f.round)).or_default();
            r.mgmt_fee = Some(f.mgmt_fee);
            r.perf_fee = Some(f.perf_fee);
        }
        ChainEvent::VaultRoundFinalized(f) => {
            if let Some(v) = inner.vaults.get_mut(&f.vault_id) {
                v.round = f.round + 1;
                v.current_bucket = None;
                v.latest_pps = Some(f.pps);
                // Supply after the finalize's burn + mint. `f.shares` is the
                // pps denominator (pre burn/mint).
                v.total_shares = f
                    .shares
                    .saturating_sub(f.shares_burned)
                    .saturating_add(f.shares_minted);
                v.pending_deposits = v.pending_deposits.saturating_sub(f.deposits_processed);
            }
            let r = inner.vault_rounds.entry((f.vault_id, f.round)).or_default();
            r.pps = Some(f.pps);
            r.aum = Some(f.aum);
            r.shares = Some(f.shares);
            r.premium_collected = Some(f.premium_collected);
            r.finalized_at_ms = Some(timestamp_ms);
        }
        ChainEvent::VaultDepositsPaused(p) => {
            if let Some(v) = inner.vaults.get_mut(&p.vault_id) {
                v.deposits_paused = p.paused;
            }
        }
        ChainEvent::FeeUpdated(_)
        | ChainEvent::TreasuryWithdrawn(_)
        | ChainEvent::VaultPositionRedeemed(_)
        | ChainEvent::SwapRfqCreated(_)
        | ChainEvent::SwapRfqBid(_)
        | ChainEvent::SwapRfqSettled(_)
        | ChainEvent::SwapRfqUnfilled(_)
        | ChainEvent::VaultConfigUpdated(_) => {}
    }
}

fn apply_bucket_created(inner: &mut Inner, b: &BucketCreated) {
    inner.buckets.insert(
        b.bucket_id,
        BucketState {
            asset_type: b.asset_type.clone(),
            settlement_type: b.settlement_type.clone(),
            call_type: b.call_type.clone(),
            strike: b.strike,
            strike_scale: b.strike_scale,
            expiry_ms: b.expiry_ms,
            total_written: 0,
            exercise_cursor: 0,
            cleaned: false,
            invalidated: false,
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
    // Positions: the Position object goes to `position_recipient`.
    inner.positions.insert(
        (w.bucket_id, w.range_start),
        PositionState {
            bucket_id: w.bucket_id,
            object_id: w.position_id,
            recipient: w.position_recipient,
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
    use protocol_types::events::{
        AccountCreated, AccountDeposit, BucketCreated, BucketInvalidated, BucketRevalidated,
    };

    fn bucket_evt(id: u8) -> ChainEvent {
        ChainEvent::BucketCreated(BucketCreated {
            bucket_id: ObjectId::new([id; 32]),
            asset_type: AssetType::new("BTC"),
            settlement_type: AssetType::new("USDC"),
            call_type: AssetType::new("0x9::call_0::CALL_0"),
            expiry_ms: 1_000,
            strike: 50_000_000,
            strike_scale: 0,
        })
    }

    #[test]
    fn ingest_assigns_monotonic_sequence() {
        let store = Store::default();
        let a = store.ingest(bucket_evt(0x01), 1);
        let b = store.ingest(bucket_evt(0x02), 2);
        // Sequences start at 1 (0 means "from before the beginning").
        assert_eq!(a.sequence, 1);
        assert_eq!(b.sequence, 2);
        assert_eq!(store.latest_sequence(), 2);
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
                call_type: AssetType::new("0x9::call_0::CALL_0"),
                expiry_ms: 1_000,
                strike: 50,
                strike_scale: 0,
            }),
            1,
        );
        store.ingest(
            ChainEvent::WriteExecuted(WriteExecuted {
                bucket_id: id,
                signer_account_id: ObjectId::ZERO,
                signer_token_recipient: SuiAddress::ZERO,
                executor: SuiAddress::ZERO,
                position_id: ObjectId::new([0x88; 32]),
                position_recipient: SuiAddress::new([0x77; 32]),
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
                signing_scheme: protocol_types::SigningScheme::Ed25519,
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

    #[test]
    fn bucket_invalidation_flag_toggles_via_events() {
        let store = Store::default();
        let id = ObjectId::new([0x42; 32]);
        store.ingest(bucket_evt(0x42), 1);
        assert!(!store.bucket(&id).unwrap().invalidated);

        store.ingest(
            ChainEvent::BucketInvalidated(BucketInvalidated {
                bucket_id: id,
                at_ms: 100,
                admin: SuiAddress::new([0xa1; 32]),
                reason: b"bad config".to_vec(),
            }),
            2,
        );
        assert!(store.bucket(&id).unwrap().invalidated);

        store.ingest(
            ChainEvent::BucketRevalidated(BucketRevalidated {
                bucket_id: id,
                at_ms: 200,
                admin: SuiAddress::new([0xa1; 32]),
                reason: b"resolved".to_vec(),
            }),
            3,
        );
        assert!(!store.bucket(&id).unwrap().invalidated);
    }

    #[test]
    fn rfq_lifecycle_created_bid_settled() {
        use protocol_types::events::{RfqBid, RfqCreated, RfqSettled};
        let store = Store::default();
        let rfq = ObjectId::new([0xaa; 32]);
        let bidder = SuiAddress::new([0x01; 32]);
        let sniper = SuiAddress::new([0x02; 32]);

        store.ingest(
            ChainEvent::RfqCreated(RfqCreated {
                rfq_id: rfq,
                bucket_id: ObjectId::new([0xb1; 32]),
                origin: ObjectId::new([0xf0; 32]),
                amount: 250_000_000,
                reserve_premium: 47_619_000,
                deadline_ms: 1_000_000,
                max_deadline_ms: 1_600_000,
                min_increment_bps: 100,
            }),
            1,
        );
        assert_eq!(store.rfq(&rfq).unwrap().status, RfqStatus::Open);

        store.ingest(
            ChainEvent::RfqBid(RfqBid {
                rfq_id: rfq,
                bidder,
                call_recipient: bidder,
                premium: 50_000_000,
                previous_premium: 0,
                new_deadline_ms: 1_000_000,
            }),
            2,
        );
        // Snipe: higher bid pushes the deadline.
        store.ingest(
            ChainEvent::RfqBid(RfqBid {
                rfq_id: rfq,
                bidder: sniper,
                call_recipient: sniper,
                premium: 51_000_000,
                previous_premium: 50_000_000,
                new_deadline_ms: 1_120_000,
            }),
            3,
        );
        let s = store.rfq(&rfq).unwrap();
        assert_eq!(s.best_premium, Some(51_000_000));
        assert_eq!(s.best_bidder, Some(sniper));
        assert_eq!(s.deadline_ms, 1_120_000);

        store.ingest(
            ChainEvent::RfqSettled(RfqSettled {
                rfq_id: rfq,
                bucket_id: ObjectId::new([0xb1; 32]),
                origin: ObjectId::new([0xf0; 32]),
                winner: sniper,
                call_recipient: sniper,
                position_id: ObjectId::new([0x99; 32]),
                position_recipient: SuiAddress::new([0xf0; 32]),
                amount: 250_000_000,
                gross_premium: 51_000_000,
                fee: 510_000,
                net_premium: 50_490_000,
                range_start: 0,
                range_end: 250_000_000,
            }),
            4,
        );
        let s = store.rfq(&rfq).unwrap();
        assert_eq!(s.status, RfqStatus::Settled);
        assert_eq!(s.winner, Some(sniper));
        assert_eq!(s.net_premium, Some(50_490_000));
        assert_eq!(s.gross_premium, Some(51_000_000));
        assert_eq!(s.fee, Some(510_000));
        assert_eq!(s.position_id, Some(ObjectId::new([0x99; 32])));
    }

    #[test]
    fn vault_lifecycle_deposit_select_finalize() {
        use protocol_types::events::{
            VaultBucketSelected, VaultCreated, VaultDeposit, VaultRoundFinalized,
        };
        let store = Store::default();
        let vault = ObjectId::new([0xf0; 32]);
        let lp = SuiAddress::new([0x07; 32]);

        store.ingest(
            ChainEvent::VaultCreated(VaultCreated {
                vault_id: vault,
                underlying_type: AssetType::new("0x2::sui::SUI"),
                settlement_type: AssetType::new("0x9::usdc::USDC"),
                share_type: AssetType::new("0x9::vshare::VSHARE"),
                mgmt_fee_bps_annual: 200,
                perf_fee_bps: 1000,
                round_ms: 604_800_000,
                selling_window_ms: 43_200_000,
                min_strike_bps_over_spot: 300,
                max_strike_bps_over_spot: 6000,
            }),
            1,
        );
        store.ingest(
            ChainEvent::VaultDeposit(VaultDeposit {
                vault_id: vault,
                depositor: lp,
                round: 1,
                amount: 700_000_000,
            }),
            2,
        );
        let v = store.vault(&vault).unwrap();
        assert_eq!(v.round, 0);
        assert_eq!(v.pending_deposits, 700_000_000);

        // Genesis finalize (round 0): deposits convert, shares mint.
        store.ingest(
            ChainEvent::VaultRoundFinalized(VaultRoundFinalized {
                vault_id: vault,
                round: 0,
                pps: 1_000_000_000_000,
                aum: 0,
                shares: 0,
                premium_collected: 0,
                premium_underlying: 0,
                withdrawals_owed: 0,
                shares_burned: 0,
                deposits_processed: 700_000_000,
                shares_minted: 700_000_000,
            }),
            3,
        );
        let v = store.vault(&vault).unwrap();
        assert_eq!(v.round, 1);
        assert_eq!(v.total_shares, 700_000_000);
        assert_eq!(v.pending_deposits, 0);
        assert_eq!(v.latest_pps, Some(1_000_000_000_000));

        store.ingest(
            ChainEvent::VaultBucketSelected(VaultBucketSelected {
                vault_id: vault,
                round: 1,
                bucket_id: ObjectId::new([0xb1; 32]),
                strike: 40_500,
                strike_scale: 7,
                expiry_ms: 2_000_000,
                selling_ends_ms: 1_500_000,
                spot: 3_470_000_000_000,
                spot_scale: 12,
            }),
            4,
        );
        let v = store.vault(&vault).unwrap();
        assert_eq!(v.current_bucket, Some(ObjectId::new([0xb1; 32])));
        let r = store.vault_round(&vault, 1).unwrap();
        assert_eq!(r.strike, Some(40_500));
        assert_eq!(r.expiry_ms, Some(2_000_000));
        assert_eq!(r.pps, None, "not finalized yet");

        // Receipt aggregate for the LP's genesis deposit.
        let staged = store
            .stage_batch(
                9,
                5_000,
                vec![(
                    ChainEvent::VaultDeposit(VaultDeposit {
                        vault_id: vault,
                        depositor: lp,
                        round: 2,
                        amount: 100,
                    }),
                    "0xd".to_string(),
                    0,
                )],
            )
            .unwrap();
        assert_eq!(staged.db_batch.vaults.len(), 1);
        assert_eq!(staged.db_batch.vault_receipts.len(), 1);
        let receipt = &staged.db_batch.vault_receipts[0];
        assert_eq!(receipt.kind, RECEIPT_DEPOSIT);
        assert_eq!(receipt.round, 2);
    }

    #[test]
    fn deepbook_pool_first_wins_and_stages_one_row() {
        let store = Store::default();
        let bucket = ObjectId::new([0x42; 32]);
        store.ingest(bucket_evt(0x42), 1);

        let pool_evt = |pool: u8| {
            ChainEvent::DeepBookPoolCreated(DeepBookPoolCreated {
                pool_id: ObjectId::new([pool; 32]),
                bucket_id: bucket,
                base_asset_type: AssetType::new("0x9::call_0::CALL_0"),
                quote_asset_type: AssetType::new("USDC"),
                tick_size: 10_000,
                lot_size: 1_000,
                min_size: 10_000,
                taker_fee: 1_000_000,
                maker_fee: 500_000,
            })
        };

        let staged = store
            .stage_batch(
                2,
                1_000,
                vec![
                    (pool_evt(0xa1), "0xd1".to_string(), 0),
                    // Duplicate venue for the same bucket: kept out of both
                    // the view and the DB batch.
                    (pool_evt(0xa2), "0xd2".to_string(), 0),
                ],
            )
            .unwrap();

        assert_eq!(staged.db_batch.deepbook_pools.len(), 1);
        assert_eq!(
            staged.db_batch.deepbook_pools[0].pool_id,
            ObjectId::new([0xa1; 32]).to_hex()
        );
        let state = store.deepbook_pool(&bucket).unwrap();
        assert_eq!(state.pool_id, ObjectId::new([0xa1; 32]));
        // Both events still land in the log.
        assert_eq!(staged.db_batch.events.len(), 2);
        assert_eq!(staged.db_batch.events[0].event_type, "DeepBookPoolCreated");
    }

    #[test]
    fn stage_batch_fans_out_event_participants() {
        let store = Store::default();
        let writer = SuiAddress::new([0x77; 32]);
        let buyer = SuiAddress::new([0x22; 32]);
        let we = ChainEvent::WriteExecuted(WriteExecuted {
            bucket_id: ObjectId::new([0x11; 32]),
            signer_account_id: ObjectId::ZERO,
            signer_token_recipient: buyer,
            executor: writer,
            position_id: ObjectId::new([0x88; 32]),
            position_recipient: writer,
            call_token_recipient: buyer,
            write_amount: 10,
            gross_premium: 5,
            fee: 0,
            net_premium: 5,
            range_start: 0,
            range_end: 10,
            nonce: 1,
        });
        let staged = store
            .stage_batch(1, 1_000, vec![(we, "0xdigest".to_string(), 0)])
            .unwrap();
        let parts = &staged.db_batch.event_participants;
        let has = |addr: &SuiAddress, role: &str| {
            parts.iter().any(|p| p.address == addr.to_hex() && p.role == role)
        };
        assert!(has(&writer, "position_recipient"));
        assert!(has(&writer, "executor"));
        assert!(has(&buyer, "call_token_recipient"));
        assert!(has(&buyer, "signer_token_recipient"));
        // Account owner unknown (no AccountCreated) → no signer_account_owner row.
        assert!(!parts.iter().any(|p| p.role == "signer_account_owner"));
        assert_eq!(staged.db_batch.events.len(), 1);
    }
}
