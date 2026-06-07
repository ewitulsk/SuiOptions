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
    AccountDeposit, AccountWithdraw, BucketCreated, ChainEvent, Exercised, IndexedEvent, Redeemed,
    WriteExecuted,
};
use protocol_types::ids::{ObjectId, SuiAddress};

use crate::db::models::{
    u128_to_bigdecimal, u64_to_bigdecimal, AccountBalanceRow, AccountRow, BucketRow,
    EventParticipantRow, PositionRow,
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
    pub strike: u128,
    pub strike_scale: u8,
    pub expiry_ms: u64,
    pub total_written: u128,
    pub exercise_cursor: u128,
    pub cleaned: bool,
    pub invalidated: bool,
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
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            next_sequence: 1,
            accounts: BTreeMap::new(),
            buckets: BTreeMap::new(),
            positions: BTreeMap::new(),
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
        apply_event(&mut inner, &event);
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
            apply_event(&mut inner, &event);
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
        ChainEvent::BucketCreated(_)
        | ChainEvent::BucketCleaned(_)
        | ChainEvent::FeeUpdated(_) => {}
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
                call_option_id: ObjectId::new([0x99; 32]),
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
            call_option_id: ObjectId::new([0x99; 32]),
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
