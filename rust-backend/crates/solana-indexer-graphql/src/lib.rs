//! Just-in-time client for the **solana-indexer**'s GraphQL query API.
//!
//! The Solana twin of `crates/indexer-graphql`: every helper is one HTTP
//! round-trip to the indexer's `/graphql` listener (point lookups) or
//! `/progress` (slot-ingestion status). Solana renames vs the Sui client:
//! checkpoint → slot, tx digest → signature, RFQs → auctions; all ids are
//! base58 pubkey `String`s (byte-exact, no normalization).
//!
//! On-chain integers cross the wire as decimal strings (the GraphQL API's
//! precision-safe convention); we parse them back into `u64` / `u128` /
//! `u8` here so callers get typed values. Event payloads are the raw event
//! JSON (field names match `solana-contracts/programs/*/src/events.rs`,
//! snake_case, pubkeys base58, ints as strings) — NOT the Sui indexer's
//! tagged envelope, so `payloadContains` matches fields directly.
//!
//! Reorg posture: views are confirmed-tier; event scans accept
//! `finalized_only` to constrain to `slot <= finalizedSlot` (the
//! reorg-proof tier) for consumers folding events into their own state.

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Page size for paginated event scans. The GraphQL `events` query clamps
/// to 1..=1000; we request the max and follow `nextCursor`.
const EVENT_PAGE_LIMIT: i64 = 1000;

/// JIT client over the solana-indexer's GraphQL + progress HTTP API.
#[derive(Clone)]
pub struct IndexerClient {
    http: reqwest::Client,
    graphql_url: String,
    progress_url: String,
}

// ── domain types returned to callers ──────────────────────────────────────

/// A bucket from the indexer's materialized view, numbers parsed.
#[derive(Clone, Debug)]
pub struct Bucket {
    pub bucket_id: String,
    pub underlying_mint: String,
    pub settlement_mint: String,
    /// Mint of the per-bucket fungible option token.
    pub option_mint: String,
    /// "call" or "put".
    pub option_kind: String,
    pub strike: u128,
    pub strike_scale: u8,
    pub expiry_ms: u64,
    pub total_written: u128,
    pub exercise_cursor: u128,
    pub cleaned: bool,
    pub invalidated: bool,
}

/// An MM account's registered signing key + per-mint balances.
/// `signing_scheme` is the on-chain u8 tag (0=Ed25519, the only scheme in
/// program v1).
#[derive(Clone, Debug)]
pub struct Account {
    pub account_id: String,
    pub owner: String,
    pub signing_scheme: u8,
    pub signing_pubkey: Vec<u8>,
    pub balances: BTreeMap<String, u64>,
}

impl Account {
    /// On-chain balance for `mint`, 0 if none recorded. Callers subtract
    /// their own local reservations to get spendable balance.
    pub fn balance(&self, mint: &str) -> u64 {
        self.balances.get(mint).copied().unwrap_or(0)
    }
}

/// An enriched position (position row joined to its bucket + mint
/// provenance). Positions are fresh keypairs on Solana — `position_id` is
/// the account's pubkey.
#[derive(Clone, Debug)]
pub struct Position {
    pub position_id: String,
    pub bucket_id: String,
    pub recipient: String,
    pub range_start: u128,
    pub range_end: u128,
    /// "call" or "put".
    pub option_kind: String,
    pub underlying_mint: String,
    pub settlement_mint: String,
    pub option_mint: String,
    pub strike: u128,
    pub strike_scale: u8,
    pub expiry_ms: u64,
    pub total_written: u128,
    pub exercise_cursor: u128,
    pub premium_received: u64,
    /// `None` for collateralized (non-quote) writes.
    pub mm_account_id: Option<String>,
    pub signature: String,
    pub minted_at_ms: u64,
}

/// One venue auction from the indexer's materialized view.
#[derive(Clone, Debug)]
pub struct Auction {
    pub auction_id: String,
    /// `swap` | `covered_call` | `cash_secured_put`.
    pub mode: String,
    /// `None` for pure swaps.
    pub bucket_id: Option<String>,
    pub creator: String,
    pub escrow_mint: String,
    pub bid_mint: String,
    pub amount: u64,
    pub notional: u64,
    pub reserve_bid: u64,
    pub deadline_ms: u64,
    pub max_deadline_ms: u64,
    pub min_increment_bps: u64,
    pub settle_authority: Option<String>,
    pub best_bid: Option<u64>,
    pub best_bidder: Option<String>,
    /// `open` | `settled` | `unsold`.
    pub status: String,
    pub winner: Option<String>,
    pub token_recipient: Option<String>,
    pub position_id: Option<String>,
    /// Bid before the protocol fee (settled auctions only).
    pub gross_bid: Option<u64>,
    /// Protocol fee taken at settle (settled auctions only).
    pub fee: Option<u64>,
    pub net_proceeds: Option<u64>,
    pub bid_refunded: Option<bool>,
}

/// One bid in an auction's history.
#[derive(Clone, Debug)]
pub struct AuctionBid {
    pub auction_id: String,
    pub sequence: u64,
    pub bidder: String,
    pub token_recipient: String,
    pub bid: u64,
    pub previous_bid: u64,
    pub deadline_ms: u64,
}

/// One vault's headline state.
#[derive(Clone, Debug)]
pub struct Vault {
    pub vault_id: String,
    pub underlying_mint: String,
    pub settlement_mint: String,
    pub share_mint: String,
    /// Current round (last finalized + 1; 0 = pre-genesis).
    pub round: u64,
    pub current_bucket: Option<String>,
    pub latest_pps: Option<u128>,
    pub total_shares: u64,
    pub pending_deposits: u64,
    pub deposits_paused: bool,
    pub mgmt_fee_bps_annual: Option<u64>,
    pub perf_fee_bps: Option<u64>,
    pub round_ms: Option<u64>,
    pub selling_window_ms: Option<u64>,
    pub min_strike_bps_over_spot: Option<u64>,
    pub max_strike_bps_over_spot: Option<u64>,
}

/// One round of a vault's track record. Selection fields land at
/// `select_bucket`; pps/aum/premium at finalize.
#[derive(Clone, Debug)]
pub struct VaultRound {
    pub vault_id: String,
    pub round: u64,
    pub bucket_id: Option<String>,
    pub strike: Option<u128>,
    pub strike_scale: Option<u8>,
    pub expiry_ms: Option<u64>,
    pub selling_ends_ms: Option<u64>,
    pub spot: Option<u128>,
    pub spot_scale: Option<u8>,
    pub pps: Option<u128>,
    pub aum: Option<u64>,
    pub shares: Option<u64>,
    pub premium_collected: Option<u64>,
    pub mgmt_fee: Option<u64>,
    pub perf_fee: Option<u64>,
    pub finalized_at_ms: Option<u64>,
}

/// One realized-APY point: annualized pps growth landing at a finalized
/// round's finalize time.
#[derive(Clone, Debug)]
pub struct VaultApyPoint {
    pub round: u64,
    pub t_ms: u64,
    pub apy: f64,
}

/// One (vault, owner, round, kind) receipt aggregate. Receipts are
/// fresh-keypair accounts; the aggregate is keyed by owner.
#[derive(Clone, Debug)]
pub struct VaultReceipt {
    pub vault_id: String,
    pub owner: String,
    pub round: u64,
    /// `deposit` | `withdraw`.
    pub kind: String,
    pub amount: u64,
    pub settled: u64,
}

/// One indexed event with its payload left as raw event JSON (base58
/// pubkeys, decimal-string ints, snake_case fields per the programs'
/// `events.rs`).
#[derive(Clone, Debug)]
pub struct IndexedEvent {
    pub sequence: u64,
    pub slot: u64,
    pub signature: String,
    pub event_type: String,
    pub timestamp_ms: u64,
    pub payload: serde_json::Value,
}

impl IndexedEvent {
    /// A decimal-string integer field out of the payload.
    pub fn payload_u64(&self, field: &str) -> Result<u64> {
        payload_u64(&self.payload, field)
    }

    /// A string field (pubkey, mode, …) out of the payload.
    pub fn payload_str(&self, field: &str) -> Result<&str> {
        self.payload
            .get(field)
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow!("event {} payload missing string field {field}", self.event_type)
            })
    }
}

/// Slot-ingestion progress (the `/progress` REST endpoint). `Serialize` so
/// a proxying service can re-emit it unchanged. `ms_since_last_slot`
/// beyond a few seconds means the stream is stalled (Solana confirms ~2–3
/// slots/sec) — treat indexer data as stale.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Progress {
    pub start_slot: u64,
    pub current_slot: u64,
    /// The reorg-proof watermark: events with `slot <= finalized_slot` are
    /// immutable truth.
    pub finalized_slot: u64,
    pub rate_slots_per_sec: f64,
    /// `None` until the first slot lands.
    pub ms_since_last_slot: Option<i64>,
}

// ── event filter ───────────────────────────────────────────────────────────

/// Typed builder for the indexer's `EventFilterInput`. Everything set is
/// ANDed. `account` / `bucket` / `vault` / `auction` are server-side sugar
/// for `payloadContains: {"<field>": "<pubkey>"}` — the field names match
/// the Solana events (`bucket`, not `bucket_id`).
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventFilter {
    #[serde(skip_serializing_if = "Option::is_none")]
    event_type: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    participant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bucket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vault: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload_contains: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sequence_gt: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    slot_gte: Option<u64>,
}

impl EventFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Restrict to these event tags (`WriteExecuted`, `AuctionSettled`, …).
    pub fn event_types<I, S>(mut self, types: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.event_type = Some(types.into_iter().map(Into::into).collect());
        self
    }

    /// Address in ANY payload role (executor, recipient, bidder, …).
    pub fn participant(mut self, address: impl Into<String>) -> Self {
        self.participant = Some(address.into());
        self
    }

    /// Sugar for `payloadContains: {"account": <pubkey>}`.
    pub fn account(mut self, account: impl Into<String>) -> Self {
        self.account = Some(account.into());
        self
    }

    /// Sugar for `payloadContains: {"bucket": <pubkey>}`.
    pub fn bucket(mut self, bucket: impl Into<String>) -> Self {
        self.bucket = Some(bucket.into());
        self
    }

    /// Sugar for `payloadContains: {"vault": <pubkey>}`.
    pub fn vault(mut self, vault: impl Into<String>) -> Self {
        self.vault = Some(vault.into());
        self
    }

    /// Sugar for `payloadContains: {"auction": <pubkey>}`.
    pub fn auction(mut self, auction: impl Into<String>) -> Self {
        self.auction = Some(auction.into());
        self
    }

    /// Arbitrary JSONB `@>` containment on the raw event payload. Remember
    /// numeric payload values are decimal strings
    /// (`{"nonce": "42"}`, not `{"nonce": 42}`).
    pub fn payload_contains(mut self, value: serde_json::Value) -> Self {
        self.payload_contains = Some(value);
        self
    }

    pub fn sequence_gt(mut self, sequence: u64) -> Self {
        self.sequence_gt = Some(sequence);
        self
    }

    pub fn slot_gte(mut self, slot: u64) -> Self {
        self.slot_gte = Some(slot);
        self
    }
}

/// Order of an `events` page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventOrder {
    SequenceAsc,
    SequenceDesc,
}

impl EventOrder {
    fn as_gql(self) -> &'static str {
        match self {
            EventOrder::SequenceAsc => "SEQUENCE_ASC",
            EventOrder::SequenceDesc => "SEQUENCE_DESC",
        }
    }
}

const EVENT_NODE_FIELDS: &str = "sequence slot signature eventType payload timestampMs";

impl IndexerClient {
    /// `graphql_url` is the full `…/graphql` endpoint; the progress URL is
    /// derived as a sibling `…/progress` (same host:port).
    pub fn new(graphql_url: String) -> Self {
        let base = graphql_url
            .strip_suffix("/graphql")
            .unwrap_or(&graphql_url)
            .trim_end_matches('/');
        let progress_url = format!("{base}/progress");
        Self {
            http: reqwest::Client::new(),
            graphql_url,
            progress_url,
        }
    }

    // ── point lookups ─────────────────────────────────────────────────────

    /// One bucket by id, or `None` if the indexer doesn't know it.
    pub async fn bucket(&self, bucket_id: &str) -> Result<Option<Bucket>> {
        const Q: &str = "query($id:String!){bucket(id:$id){bucketId underlyingMint \
            settlementMint optionMint optionKind strikeRaw strikeScale expiryMs \
            totalWrittenRaw exerciseCursorRaw cleaned invalidated}}";
        let data: BucketWrap = self.gql(Q, json!({ "id": bucket_id })).await?;
        data.bucket.map(Bucket::try_from).transpose()
    }

    /// Buckets matching the filters (all ANDed). `active_only` drops
    /// cleaned buckets.
    #[allow(clippy::too_many_arguments)]
    pub async fn buckets(
        &self,
        active_only: bool,
        ids: Option<&[String]>,
        underlying_mint: Option<&str>,
        settlement_mint: Option<&str>,
        expiry_ms: Option<u64>,
        option_kind: Option<&str>,
    ) -> Result<Vec<Bucket>> {
        const Q: &str = "query($a:Boolean,$ids:[String!],$u:String,$s:String,$e:String,$k:String){\
            buckets(activeOnly:$a,ids:$ids,underlyingMint:$u,settlementMint:$s,expiryMs:$e,\
            optionKind:$k){bucketId underlyingMint settlementMint optionMint optionKind \
            strikeRaw strikeScale expiryMs totalWrittenRaw exerciseCursorRaw cleaned invalidated}}";
        let vars = json!({
            "a": active_only,
            "ids": ids,
            "u": underlying_mint,
            "s": settlement_mint,
            "e": expiry_ms.map(|e| e.to_string()),
            "k": option_kind,
        });
        let data: BucketsWrap = self.gql(Q, vars).await?;
        data.buckets.into_iter().map(Bucket::try_from).collect()
    }

    /// One MM account (signing key + balances), or `None` if unknown.
    pub async fn account(&self, account_id: &str) -> Result<Option<Account>> {
        const Q: &str = "query($id:String!){account(id:$id){accountId owner signingScheme \
            signingPubkeyHex balances{mint balanceRaw}}}";
        let data: AccountWrap = self.gql(Q, json!({ "id": account_id })).await?;
        data.account.map(Account::try_from).transpose()
    }

    /// Enriched positions for a set of position account pubkeys. Unknown
    /// ids are simply absent from the result.
    pub async fn positions(&self, ids: &[String]) -> Result<Vec<Position>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        const Q: &str = "query($ids:[String!]!){positions(ids:$ids){positionId bucketId \
            recipient rangeStartRaw rangeEndRaw optionKind underlyingMint settlementMint \
            optionMint strikeRaw strikeScale expiryMs totalWrittenRaw exerciseCursorRaw \
            premiumReceivedRaw mmAccountId signature mintedAtMs}}";
        let data: PositionsWrap = self.gql(Q, json!({ "ids": ids })).await?;
        data.positions.into_iter().map(Position::try_from).collect()
    }

    /// Enriched positions held by `recipient` (mint-time owner-of-record).
    pub async fn positions_by_recipient(&self, recipient: &str) -> Result<Vec<Position>> {
        const Q: &str = "query($r:String!){positionsByRecipient(recipient:$r){positionId \
            bucketId recipient rangeStartRaw rangeEndRaw optionKind underlyingMint \
            settlementMint optionMint strikeRaw strikeScale expiryMs totalWrittenRaw \
            exerciseCursorRaw premiumReceivedRaw mmAccountId signature mintedAtMs}}";
        let data: PositionsByRecipientWrap = self.gql(Q, json!({ "r": recipient })).await?;
        data.positions_by_recipient
            .into_iter()
            .map(Position::try_from)
            .collect()
    }

    // ── auction / vault views ─────────────────────────────────────────────

    /// Venue auctions, optionally filtered by status
    /// (`open` | `settled` | `unsold`), mode
    /// (`swap` | `covered_call` | `cash_secured_put`), bucket, or creator.
    pub async fn auctions(
        &self,
        status: Option<&str>,
        mode: Option<&str>,
        bucket_id: Option<&str>,
        creator: Option<&str>,
    ) -> Result<Vec<Auction>> {
        const Q: &str = "query($s:String,$m:String,$b:String,$c:String){\
            auctions(status:$s,mode:$m,bucketId:$b,creator:$c){auctionId mode bucketId \
            creator escrowMint bidMint amountRaw notionalRaw reserveBidRaw deadlineMs \
            maxDeadlineMs minIncrementBps settleAuthority bestBidRaw bestBidder status \
            winner tokenRecipient positionId grossBidRaw feeRaw netProceedsRaw bidRefunded}}";
        let vars = json!({ "s": status, "m": mode, "b": bucket_id, "c": creator });
        let data: AuctionsWrap = self.gql(Q, vars).await?;
        data.auctions.into_iter().map(Auction::try_from).collect()
    }

    /// Bid history for one auction, ascending.
    pub async fn auction_bids(&self, auction_id: &str) -> Result<Vec<AuctionBid>> {
        const Q: &str = "query($id:String!){auctionBids(auctionId:$id){auctionId sequence \
            bidder tokenRecipient bidRaw previousBidRaw deadlineMs}}";
        let data: AuctionBidsWrap = self.gql(Q, json!({ "id": auction_id })).await?;
        data.auction_bids
            .into_iter()
            .map(AuctionBid::try_from)
            .collect()
    }

    /// All covered-call vaults.
    pub async fn vaults(&self) -> Result<Vec<Vault>> {
        const Q: &str = "query{vaults{vaultId underlyingMint settlementMint shareMint round \
            currentBucket latestPpsRaw totalSharesRaw pendingDepositsRaw depositsPaused \
            mgmtFeeBpsAnnual perfFeeBps roundMs sellingWindowMs minStrikeBpsOverSpot \
            maxStrikeBpsOverSpot}}";
        let data: VaultsWrap = self.gql(Q, json!({})).await?;
        data.vaults.into_iter().map(Vault::try_from).collect()
    }

    /// One vault by id, or `None` if unknown.
    pub async fn vault(&self, vault_id: &str) -> Result<Option<Vault>> {
        const Q: &str = "query($id:String!){vault(id:$id){vaultId underlyingMint \
            settlementMint shareMint round currentBucket latestPpsRaw totalSharesRaw \
            pendingDepositsRaw depositsPaused mgmtFeeBpsAnnual perfFeeBps roundMs \
            sellingWindowMs minStrikeBpsOverSpot maxStrikeBpsOverSpot}}";
        let data: VaultWrap = self.gql(Q, json!({ "id": vault_id })).await?;
        data.vault.map(Vault::try_from).transpose()
    }

    /// One vault's round history, ascending (the track record).
    pub async fn vault_rounds(&self, vault_id: &str) -> Result<Vec<VaultRound>> {
        const Q: &str = "query($id:String!){vaultRounds(vaultId:$id){vaultId round bucketId \
            strikeRaw strikeScale expiryMs sellingEndsMs spotRaw spotScale ppsRaw aumRaw \
            sharesRaw premiumCollectedRaw mgmtFeeRaw perfFeeRaw finalizedAtMs}}";
        let data: VaultRoundsWrap = self.gql(Q, json!({ "id": vault_id })).await?;
        data.vault_rounds
            .into_iter()
            .map(VaultRound::try_from)
            .collect()
    }

    /// One vault's realized-APY series (annualized pps growth per
    /// finalized round), computed indexer-side. Empty until two rounds
    /// have finalized.
    pub async fn vault_apy(&self, vault_id: &str) -> Result<Vec<VaultApyPoint>> {
        const Q: &str = "query($id:String!){vaultApy(vaultId:$id){round tMs apy}}";
        let data: VaultApyWrap = self.gql(Q, json!({ "id": vault_id })).await?;
        data.vault_apy
            .into_iter()
            .map(VaultApyPoint::try_from)
            .collect()
    }

    /// Receipt aggregates for one vault, optionally scoped to an owner.
    pub async fn vault_receipts(
        &self,
        vault_id: &str,
        owner: Option<&str>,
    ) -> Result<Vec<VaultReceipt>> {
        const Q: &str = "query($id:String!,$o:String){vaultReceipts(vaultId:$id,owner:$o){\
            vaultId owner round kind amountRaw settledRaw}}";
        let vars = json!({ "id": vault_id, "o": owner });
        let data: VaultReceiptsWrap = self.gql(Q, vars).await?;
        data.vault_receipts
            .into_iter()
            .map(VaultReceipt::try_from)
            .collect()
    }

    // ── event-log scans ───────────────────────────────────────────────────

    /// One page of the generalized event log. `limit` clamps server-side
    /// to 1..=1000; pass the returned cursor back as `after` for the next
    /// page. `finalized_only` constrains to `slot <= finalizedSlot` — the
    /// reorg-proof tier.
    pub async fn events(
        &self,
        filter: Option<&EventFilter>,
        order: EventOrder,
        limit: i64,
        after: Option<&str>,
        finalized_only: bool,
    ) -> Result<(Vec<IndexedEvent>, Option<String>)> {
        let q = format!(
            "query($f:EventFilterInput,$o:EventOrder,$limit:Int,$after:String,$fin:Boolean){{\
             events(filter:$f,order:$o,limit:$limit,after:$after,finalizedOnly:$fin){{\
             nodes{{{EVENT_NODE_FIELDS}}} nextCursor}}}}"
        );
        let vars = json!({
            "f": filter,
            "o": order.as_gql(),
            "limit": limit,
            "after": after,
            "fin": finalized_only,
        });
        let data: EventsWrap = self.gql(&q, vars).await?;
        let nodes = data
            .events
            .nodes
            .into_iter()
            .map(IndexedEvent::try_from)
            .collect::<Result<Vec<_>>>()?;
        Ok((nodes, data.events.next_cursor))
    }

    /// Highest persisted sequence (0 if the log is empty) — the poll
    /// high-water mark consumers compare their own cursor against.
    pub async fn head_sequence(&self) -> Result<u64> {
        let (nodes, _) = self
            .events(None, EventOrder::SequenceDesc, 1, None, false)
            .await?;
        Ok(nodes.first().map(|n| n.sequence).unwrap_or(0))
    }

    /// Paginate the `events` query (ascending) for `filter`, starting
    /// after sequence `after`, following `nextCursor` until exhausted.
    pub async fn scan_events(
        &self,
        filter: &EventFilter,
        after: u64,
        finalized_only: bool,
    ) -> Result<Vec<IndexedEvent>> {
        let mut cursor: Option<String> = if after == 0 {
            None
        } else {
            Some(after.to_string())
        };
        let mut out = Vec::new();
        loop {
            let (nodes, next_cursor) = self
                .events(
                    Some(filter),
                    EventOrder::SequenceAsc,
                    EVENT_PAGE_LIMIT,
                    cursor.as_deref(),
                    finalized_only,
                )
                .await?;
            out.extend(nodes);
            match next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok(out)
    }

    /// All `WriteExecuted` events for MM account `account` with
    /// `sequence > after`, ascending. Backs the quoting-service's
    /// reservation reconciliation. Returns `(sequence, nonce)` pairs.
    pub async fn write_executed_for_account_since(
        &self,
        account: &str,
        after: u64,
    ) -> Result<Vec<(u64, u64)>> {
        // The stored payload is the raw event JSON, so the JSONB `@>`
        // match hits the event's own field names: `signer_account` (not
        // the Sui `signer_account_id`), no envelope nesting.
        let filter = EventFilter::new()
            .event_types(["WriteExecuted"])
            .payload_contains(json!({ "signer_account": account }));
        let events = self.scan_events(&filter, after, false).await?;
        events
            .iter()
            .map(|ev| Ok((ev.sequence, ev.payload_u64("nonce")?)))
            .collect()
    }

    /// All `WriteExecuted` events whose `call_token_recipient` is
    /// `wallet`, ascending. Backs the api-service option-token "lot"
    /// provenance list.
    pub async fn write_executed_for_recipient(&self, wallet: &str) -> Result<Vec<IndexedEvent>> {
        let filter = EventFilter::new()
            .event_types(["WriteExecuted"])
            .payload_contains(json!({ "call_token_recipient": wallet }));
        self.scan_events(&filter, 0, false).await
    }

    /// All `PutWriteExecuted` events for MM account `account` with
    /// `sequence > after`, ascending. The put-side mirror of
    /// [`write_executed_for_account_since`](Self::write_executed_for_account_since).
    /// Returns `(sequence, nonce)` pairs.
    pub async fn put_write_executed_for_account_since(
        &self,
        account: &str,
        after: u64,
    ) -> Result<Vec<(u64, u64)>> {
        let filter = EventFilter::new()
            .event_types(["PutWriteExecuted"])
            .payload_contains(json!({ "signer_account": account }));
        let events = self.scan_events(&filter, after, false).await?;
        events
            .iter()
            .map(|ev| Ok((ev.sequence, ev.payload_u64("nonce")?)))
            .collect()
    }

    /// All `PutWriteExecuted` events whose `put_token_recipient` is
    /// `wallet`, ascending. The put-side mirror of
    /// [`write_executed_for_recipient`](Self::write_executed_for_recipient).
    pub async fn put_write_executed_for_recipient(
        &self,
        wallet: &str,
    ) -> Result<Vec<IndexedEvent>> {
        let filter = EventFilter::new()
            .event_types(["PutWriteExecuted"])
            .payload_contains(json!({ "put_token_recipient": wallet }));
        self.scan_events(&filter, 0, false).await
    }

    /// Every event `wallet` participated in (any payload role), ascending
    /// by sequence. Backs the activity feed.
    pub async fn events_for_participant(&self, wallet: &str) -> Result<Vec<IndexedEvent>> {
        let filter = EventFilter::new().participant(wallet);
        self.scan_events(&filter, 0, false).await
    }

    // ── progress ──────────────────────────────────────────────────────────

    /// Slot-ingestion progress (`GET /progress`).
    pub async fn progress(&self) -> Result<Progress> {
        let resp = observability::client::instrumented("solana-indexer", "GET /progress", |h| {
            self.http.get(&self.progress_url).headers(h).send()
        })
        .await?
        .error_for_status()?;
        Ok(resp.json().await?)
    }

    // ── transport ─────────────────────────────────────────────────────────

    /// POST a GraphQL query, unwrap `data` (or surface `errors`).
    async fn gql<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<T> {
        let body = json!({ "query": query, "variables": variables });
        let resp = observability::client::instrumented("solana-indexer", "POST /graphql", |h| {
            self.http
                .post(&self.graphql_url)
                .headers(h)
                .json(&body)
                .send()
        })
        .await
        .context("sending graphql request")?
        .error_for_status()
        .context("graphql http status")?;
        let parsed: GqlEnvelope<T> = resp.json().await.context("decoding graphql response")?;
        if let Some(errors) = parsed.errors {
            if !errors.is_empty() {
                bail!("solana-indexer graphql errors: {errors:?}");
            }
        }
        parsed
            .data
            .ok_or_else(|| anyhow!("graphql response had neither data nor errors"))
    }
}

// ── wire types ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct GqlEnvelope<T> {
    data: Option<T>,
    errors: Option<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
struct BucketWrap {
    bucket: Option<BucketJson>,
}
#[derive(Deserialize)]
struct BucketsWrap {
    buckets: Vec<BucketJson>,
}
#[derive(Deserialize)]
struct AccountWrap {
    account: Option<AccountJson>,
}
#[derive(Deserialize)]
struct PositionsWrap {
    positions: Vec<PositionJson>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PositionsByRecipientWrap {
    positions_by_recipient: Vec<PositionJson>,
}
#[derive(Deserialize)]
struct AuctionsWrap {
    auctions: Vec<AuctionJson>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuctionBidsWrap {
    auction_bids: Vec<AuctionBidJson>,
}
#[derive(Deserialize)]
struct VaultsWrap {
    vaults: Vec<VaultJson>,
}
#[derive(Deserialize)]
struct VaultWrap {
    vault: Option<VaultJson>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VaultRoundsWrap {
    vault_rounds: Vec<VaultRoundJson>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VaultApyWrap {
    vault_apy: Vec<VaultApyJson>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VaultReceiptsWrap {
    vault_receipts: Vec<VaultReceiptJson>,
}
#[derive(Deserialize)]
struct EventsWrap {
    events: EventConnectionJson,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventConnectionJson {
    nodes: Vec<EventNodeJson>,
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventNodeJson {
    sequence: String,
    slot: String,
    signature: String,
    event_type: String,
    timestamp_ms: String,
    payload: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BucketJson {
    bucket_id: String,
    underlying_mint: String,
    settlement_mint: String,
    option_mint: String,
    option_kind: String,
    strike_raw: String,
    strike_scale: i32,
    expiry_ms: String,
    total_written_raw: String,
    exercise_cursor_raw: String,
    cleaned: bool,
    invalidated: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BalanceJson {
    mint: String,
    balance_raw: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountJson {
    account_id: String,
    owner: String,
    signing_scheme: i32,
    signing_pubkey_hex: String,
    balances: Vec<BalanceJson>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PositionJson {
    position_id: String,
    bucket_id: String,
    recipient: String,
    range_start_raw: String,
    range_end_raw: String,
    option_kind: String,
    underlying_mint: String,
    settlement_mint: String,
    option_mint: String,
    strike_raw: String,
    strike_scale: i32,
    expiry_ms: String,
    total_written_raw: String,
    exercise_cursor_raw: String,
    premium_received_raw: String,
    mm_account_id: Option<String>,
    signature: String,
    minted_at_ms: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuctionJson {
    auction_id: String,
    mode: String,
    bucket_id: Option<String>,
    creator: String,
    escrow_mint: String,
    bid_mint: String,
    amount_raw: String,
    notional_raw: String,
    reserve_bid_raw: String,
    deadline_ms: String,
    max_deadline_ms: String,
    min_increment_bps: String,
    settle_authority: Option<String>,
    best_bid_raw: Option<String>,
    best_bidder: Option<String>,
    status: String,
    winner: Option<String>,
    token_recipient: Option<String>,
    position_id: Option<String>,
    gross_bid_raw: Option<String>,
    fee_raw: Option<String>,
    net_proceeds_raw: Option<String>,
    bid_refunded: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuctionBidJson {
    auction_id: String,
    sequence: String,
    bidder: String,
    token_recipient: String,
    bid_raw: String,
    previous_bid_raw: String,
    deadline_ms: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VaultJson {
    vault_id: String,
    underlying_mint: String,
    settlement_mint: String,
    share_mint: String,
    round: String,
    current_bucket: Option<String>,
    latest_pps_raw: Option<String>,
    total_shares_raw: String,
    pending_deposits_raw: String,
    deposits_paused: bool,
    mgmt_fee_bps_annual: Option<String>,
    perf_fee_bps: Option<String>,
    round_ms: Option<String>,
    selling_window_ms: Option<String>,
    min_strike_bps_over_spot: Option<String>,
    max_strike_bps_over_spot: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VaultRoundJson {
    vault_id: String,
    round: String,
    bucket_id: Option<String>,
    strike_raw: Option<String>,
    strike_scale: Option<i32>,
    expiry_ms: Option<String>,
    selling_ends_ms: Option<String>,
    spot_raw: Option<String>,
    spot_scale: Option<i32>,
    pps_raw: Option<String>,
    aum_raw: Option<String>,
    shares_raw: Option<String>,
    premium_collected_raw: Option<String>,
    mgmt_fee_raw: Option<String>,
    perf_fee_raw: Option<String>,
    finalized_at_ms: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VaultApyJson {
    round: String,
    t_ms: String,
    apy: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VaultReceiptJson {
    vault_id: String,
    owner: String,
    round: String,
    kind: String,
    amount_raw: String,
    settled_raw: String,
}

// ── parsing wire → domain ──────────────────────────────────────────────────

fn parse_u128(s: &str) -> Result<u128> {
    s.parse().map_err(|e| anyhow!("bad u128 {s:?}: {e}"))
}
fn parse_u64(s: &str) -> Result<u64> {
    s.parse().map_err(|e| anyhow!("bad u64 {s:?}: {e}"))
}
fn parse_u8(v: i32) -> Result<u8> {
    u8::try_from(v).map_err(|_| anyhow!("value {v} out of u8 range"))
}

/// A decimal-string integer field out of a raw event payload.
fn payload_u64(payload: &serde_json::Value, field: &str) -> Result<u64> {
    let s = payload
        .get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("payload missing decimal-string field {field}"))?;
    parse_u64(s)
}

fn decode_hex(s: &str) -> Result<Vec<u8>> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if s.len() % 2 != 0 {
        bail!("odd-length hex string");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| anyhow!("bad hex: {e}")))
        .collect()
}

impl TryFrom<BucketJson> for Bucket {
    type Error = anyhow::Error;
    fn try_from(b: BucketJson) -> Result<Self> {
        Ok(Bucket {
            bucket_id: b.bucket_id,
            underlying_mint: b.underlying_mint,
            settlement_mint: b.settlement_mint,
            option_mint: b.option_mint,
            option_kind: b.option_kind,
            strike: parse_u128(&b.strike_raw)?,
            strike_scale: parse_u8(b.strike_scale)?,
            expiry_ms: parse_u64(&b.expiry_ms)?,
            total_written: parse_u128(&b.total_written_raw)?,
            exercise_cursor: parse_u128(&b.exercise_cursor_raw)?,
            cleaned: b.cleaned,
            invalidated: b.invalidated,
        })
    }
}

impl TryFrom<AccountJson> for Account {
    type Error = anyhow::Error;
    fn try_from(a: AccountJson) -> Result<Self> {
        let mut balances = BTreeMap::new();
        for b in a.balances {
            balances.insert(b.mint, parse_u64(&b.balance_raw)?);
        }
        Ok(Account {
            account_id: a.account_id,
            owner: a.owner,
            signing_scheme: parse_u8(a.signing_scheme)?,
            signing_pubkey: decode_hex(&a.signing_pubkey_hex)?,
            balances,
        })
    }
}

impl TryFrom<PositionJson> for Position {
    type Error = anyhow::Error;
    fn try_from(p: PositionJson) -> Result<Self> {
        Ok(Position {
            position_id: p.position_id,
            bucket_id: p.bucket_id,
            recipient: p.recipient,
            range_start: parse_u128(&p.range_start_raw)?,
            range_end: parse_u128(&p.range_end_raw)?,
            option_kind: p.option_kind,
            underlying_mint: p.underlying_mint,
            settlement_mint: p.settlement_mint,
            option_mint: p.option_mint,
            strike: parse_u128(&p.strike_raw)?,
            strike_scale: parse_u8(p.strike_scale)?,
            expiry_ms: parse_u64(&p.expiry_ms)?,
            total_written: parse_u128(&p.total_written_raw)?,
            exercise_cursor: parse_u128(&p.exercise_cursor_raw)?,
            premium_received: parse_u64(&p.premium_received_raw)?,
            mm_account_id: p.mm_account_id,
            signature: p.signature,
            minted_at_ms: parse_u64(&p.minted_at_ms)?,
        })
    }
}

impl TryFrom<AuctionJson> for Auction {
    type Error = anyhow::Error;
    fn try_from(a: AuctionJson) -> Result<Self> {
        Ok(Auction {
            auction_id: a.auction_id,
            mode: a.mode,
            bucket_id: a.bucket_id,
            creator: a.creator,
            escrow_mint: a.escrow_mint,
            bid_mint: a.bid_mint,
            amount: parse_u64(&a.amount_raw)?,
            notional: parse_u64(&a.notional_raw)?,
            reserve_bid: parse_u64(&a.reserve_bid_raw)?,
            deadline_ms: parse_u64(&a.deadline_ms)?,
            max_deadline_ms: parse_u64(&a.max_deadline_ms)?,
            min_increment_bps: parse_u64(&a.min_increment_bps)?,
            settle_authority: a.settle_authority,
            best_bid: a.best_bid_raw.as_deref().map(parse_u64).transpose()?,
            best_bidder: a.best_bidder,
            status: a.status,
            winner: a.winner,
            token_recipient: a.token_recipient,
            position_id: a.position_id,
            gross_bid: a.gross_bid_raw.as_deref().map(parse_u64).transpose()?,
            fee: a.fee_raw.as_deref().map(parse_u64).transpose()?,
            net_proceeds: a.net_proceeds_raw.as_deref().map(parse_u64).transpose()?,
            bid_refunded: a.bid_refunded,
        })
    }
}

impl TryFrom<AuctionBidJson> for AuctionBid {
    type Error = anyhow::Error;
    fn try_from(b: AuctionBidJson) -> Result<Self> {
        Ok(AuctionBid {
            auction_id: b.auction_id,
            sequence: parse_u64(&b.sequence)?,
            bidder: b.bidder,
            token_recipient: b.token_recipient,
            bid: parse_u64(&b.bid_raw)?,
            previous_bid: parse_u64(&b.previous_bid_raw)?,
            deadline_ms: parse_u64(&b.deadline_ms)?,
        })
    }
}

impl TryFrom<VaultJson> for Vault {
    type Error = anyhow::Error;
    fn try_from(v: VaultJson) -> Result<Self> {
        Ok(Vault {
            vault_id: v.vault_id,
            underlying_mint: v.underlying_mint,
            settlement_mint: v.settlement_mint,
            share_mint: v.share_mint,
            round: parse_u64(&v.round)?,
            current_bucket: v.current_bucket,
            latest_pps: v.latest_pps_raw.as_deref().map(parse_u128).transpose()?,
            total_shares: parse_u64(&v.total_shares_raw)?,
            pending_deposits: parse_u64(&v.pending_deposits_raw)?,
            deposits_paused: v.deposits_paused,
            mgmt_fee_bps_annual: v.mgmt_fee_bps_annual.as_deref().map(parse_u64).transpose()?,
            perf_fee_bps: v.perf_fee_bps.as_deref().map(parse_u64).transpose()?,
            round_ms: v.round_ms.as_deref().map(parse_u64).transpose()?,
            selling_window_ms: v.selling_window_ms.as_deref().map(parse_u64).transpose()?,
            min_strike_bps_over_spot: v
                .min_strike_bps_over_spot
                .as_deref()
                .map(parse_u64)
                .transpose()?,
            max_strike_bps_over_spot: v
                .max_strike_bps_over_spot
                .as_deref()
                .map(parse_u64)
                .transpose()?,
        })
    }
}

impl TryFrom<VaultRoundJson> for VaultRound {
    type Error = anyhow::Error;
    fn try_from(r: VaultRoundJson) -> Result<Self> {
        Ok(VaultRound {
            vault_id: r.vault_id,
            round: parse_u64(&r.round)?,
            bucket_id: r.bucket_id,
            strike: r.strike_raw.as_deref().map(parse_u128).transpose()?,
            strike_scale: r.strike_scale.map(parse_u8).transpose()?,
            expiry_ms: r.expiry_ms.as_deref().map(parse_u64).transpose()?,
            selling_ends_ms: r.selling_ends_ms.as_deref().map(parse_u64).transpose()?,
            spot: r.spot_raw.as_deref().map(parse_u128).transpose()?,
            spot_scale: r.spot_scale.map(parse_u8).transpose()?,
            pps: r.pps_raw.as_deref().map(parse_u128).transpose()?,
            aum: r.aum_raw.as_deref().map(parse_u64).transpose()?,
            shares: r.shares_raw.as_deref().map(parse_u64).transpose()?,
            premium_collected: r
                .premium_collected_raw
                .as_deref()
                .map(parse_u64)
                .transpose()?,
            mgmt_fee: r.mgmt_fee_raw.as_deref().map(parse_u64).transpose()?,
            perf_fee: r.perf_fee_raw.as_deref().map(parse_u64).transpose()?,
            finalized_at_ms: r.finalized_at_ms.as_deref().map(parse_u64).transpose()?,
        })
    }
}

impl TryFrom<VaultApyJson> for VaultApyPoint {
    type Error = anyhow::Error;
    fn try_from(p: VaultApyJson) -> Result<Self> {
        Ok(VaultApyPoint {
            round: parse_u64(&p.round)?,
            t_ms: parse_u64(&p.t_ms)?,
            apy: p.apy,
        })
    }
}

impl TryFrom<VaultReceiptJson> for VaultReceipt {
    type Error = anyhow::Error;
    fn try_from(r: VaultReceiptJson) -> Result<Self> {
        Ok(VaultReceipt {
            vault_id: r.vault_id,
            owner: r.owner,
            round: parse_u64(&r.round)?,
            kind: r.kind,
            amount: parse_u64(&r.amount_raw)?,
            settled: parse_u64(&r.settled_raw)?,
        })
    }
}

impl TryFrom<EventNodeJson> for IndexedEvent {
    type Error = anyhow::Error;
    fn try_from(n: EventNodeJson) -> Result<Self> {
        Ok(IndexedEvent {
            sequence: parse_u64(&n.sequence).context("parsing event sequence")?,
            slot: parse_u64(&n.slot).context("parsing event slot")?,
            signature: n.signature,
            event_type: n.event_type,
            timestamp_ms: parse_u64(&n.timestamp_ms).context("parsing event timestamp")?,
            payload: n.payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_url_derived_from_graphql_url() {
        let c = IndexerClient::new("http://solana-indexer:9002/graphql".to_string());
        assert_eq!(c.progress_url, "http://solana-indexer:9002/progress");
        // Trailing-slash and bare-base forms behave too.
        let c = IndexerClient::new("http://127.0.0.1:9002/".to_string());
        assert_eq!(c.progress_url, "http://127.0.0.1:9002/progress");
    }

    #[test]
    fn event_filter_serializes_camel_case_and_skips_unset() {
        let f = EventFilter::new()
            .event_types(["WriteExecuted", "PutWriteExecuted"])
            .bucket("9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin")
            .sequence_gt(42)
            .slot_gte(351_234_000);
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "eventType": ["WriteExecuted", "PutWriteExecuted"],
                "bucket": "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin",
                "sequenceGt": 42,
                "slotGte": 351_234_000,
            })
        );

        let f = EventFilter::new()
            .participant("wallet")
            .payload_contains(serde_json::json!({ "nonce": "7" }));
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "participant": "wallet",
                "payloadContains": { "nonce": "7" },
            })
        );
    }

    #[test]
    fn decodes_bucket_fixture() {
        let env: GqlEnvelope<BucketWrap> = serde_json::from_str(
            r#"{"data":{"bucket":{
                "bucketId":"9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin",
                "underlyingMint":"So11111111111111111111111111111111111111112",
                "settlementMint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "optionMint":"opt1111111111111111111111111111111111111111",
                "optionKind":"call",
                "strikeRaw":"340282366920938463463374607431768211455",
                "strikeScale":6,
                "expiryMs":"1760000000000",
                "totalWrittenRaw":"5000000000",
                "exerciseCursorRaw":"0",
                "cleaned":false,
                "invalidated":false}}}"#,
        )
        .unwrap();
        let b = Bucket::try_from(env.data.unwrap().bucket.unwrap()).unwrap();
        assert_eq!(b.strike, u128::MAX); // full u128 range survives
        assert_eq!(b.strike_scale, 6);
        assert_eq!(b.expiry_ms, 1_760_000_000_000);
        assert_eq!(b.total_written, 5_000_000_000);
        assert_eq!(b.option_kind, "call");
        assert!(!b.cleaned && !b.invalidated);
    }

    #[test]
    fn decodes_account_fixture() {
        let env: GqlEnvelope<AccountWrap> = serde_json::from_str(
            r#"{"data":{"account":{
                "accountId":"acc111",
                "owner":"own111",
                "signingScheme":0,
                "signingPubkeyHex":"00ab",
                "balances":[
                  {"mint":"So11111111111111111111111111111111111111112","balanceRaw":"123"},
                  {"mint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","balanceRaw":"456"}
                ]}}}"#,
        )
        .unwrap();
        let a = Account::try_from(env.data.unwrap().account.unwrap()).unwrap();
        assert_eq!(a.signing_scheme, 0);
        assert_eq!(a.signing_pubkey, vec![0x00, 0xab]);
        assert_eq!(a.balance("So11111111111111111111111111111111111111112"), 123);
        assert_eq!(a.balance("unknown"), 0);
    }

    #[test]
    fn decodes_auction_fixture_with_nullables() {
        let env: GqlEnvelope<AuctionsWrap> = serde_json::from_str(
            r#"{"data":{"auctions":[{
                "auctionId":"auc111","mode":"covered_call","bucketId":"bkt111",
                "creator":"cre111","escrowMint":"m1","bidMint":"m2",
                "amountRaw":"1000","notionalRaw":"2000","reserveBidRaw":"50",
                "deadlineMs":"1760000000000","maxDeadlineMs":"1760000600000",
                "minIncrementBps":"25","settleAuthority":null,
                "bestBidRaw":"75","bestBidder":"bid111","status":"settled",
                "winner":"bid111","tokenRecipient":"rcp111","positionId":"pos111",
                "grossBidRaw":"75","feeRaw":"3","netProceedsRaw":"72","bidRefunded":null
            },{
                "auctionId":"auc222","mode":"swap","bucketId":null,
                "creator":"cre222","escrowMint":"m1","bidMint":"m2",
                "amountRaw":"10","notionalRaw":"0","reserveBidRaw":"1",
                "deadlineMs":"1","maxDeadlineMs":"2","minIncrementBps":"0",
                "settleAuthority":"sa1","bestBidRaw":null,"bestBidder":null,
                "status":"unsold","winner":null,"tokenRecipient":null,
                "positionId":null,"grossBidRaw":null,"feeRaw":null,
                "netProceedsRaw":null,"bidRefunded":true}]}}"#,
        )
        .unwrap();
        let auctions: Vec<Auction> = env
            .data
            .unwrap()
            .auctions
            .into_iter()
            .map(Auction::try_from)
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(auctions.len(), 2);
        let settled = &auctions[0];
        assert_eq!(settled.mode, "covered_call");
        assert_eq!(settled.best_bid, Some(75));
        assert_eq!(settled.net_proceeds, Some(72));
        let swap = &auctions[1];
        assert!(swap.bucket_id.is_none()); // pure swaps carry no bucket
        assert_eq!(swap.bid_refunded, Some(true));
        assert!(swap.best_bid.is_none());
    }

    #[test]
    fn decodes_vault_round_fixture() {
        let env: GqlEnvelope<VaultRoundsWrap> = serde_json::from_str(
            r#"{"data":{"vaultRounds":[{
                "vaultId":"vlt111","round":"3","bucketId":"bkt111",
                "strikeRaw":"65000000000","strikeScale":6,"expiryMs":"1760000000000",
                "sellingEndsMs":"1759990000000","spotRaw":"60000000000","spotScale":6,
                "ppsRaw":"1002000000000","aumRaw":"999","sharesRaw":"888",
                "premiumCollectedRaw":"77","mgmtFeeRaw":"1","perfFeeRaw":"2",
                "finalizedAtMs":"1760000001000"
            },{
                "vaultId":"vlt111","round":"4","bucketId":null,
                "strikeRaw":null,"strikeScale":null,"expiryMs":null,
                "sellingEndsMs":null,"spotRaw":null,"spotScale":null,
                "ppsRaw":null,"aumRaw":null,"sharesRaw":null,
                "premiumCollectedRaw":null,"mgmtFeeRaw":null,"perfFeeRaw":null,
                "finalizedAtMs":null}]}}"#,
        )
        .unwrap();
        let rounds: Vec<VaultRound> = env
            .data
            .unwrap()
            .vault_rounds
            .into_iter()
            .map(VaultRound::try_from)
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(rounds[0].pps, Some(1_002_000_000_000));
        assert_eq!(rounds[0].spot, Some(60_000_000_000));
        assert_eq!(rounds[0].spot_scale, Some(6));
        assert_eq!(rounds[0].selling_ends_ms, Some(1_759_990_000_000));
        assert_eq!(rounds[1].round, 4);
        assert!(rounds[1].pps.is_none() && rounds[1].bucket_id.is_none());
    }

    #[test]
    fn decodes_events_fixture_and_reads_payload() {
        let env: GqlEnvelope<EventsWrap> = serde_json::from_str(
            r#"{"data":{"events":{"nodes":[{
                "sequence":"17","slot":"351234567",
                "signature":"5j7s6NiJS3JAkvgkoc18WVAsiSaci2pxB2A6ueCJP4tprA2TFg9wSyTLeYouxPBJEMzJinENTkpA52YStRW5Dia7",
                "txIndex":2,"innerIxIndex":0,"program":"options_core",
                "timestampMs":"1760000000123","eventType":"WriteExecuted",
                "payload":{
                    "bucket":"bkt111","signer_account":"acc111",
                    "signer_token_recipient":"str111","executor":"exe111",
                    "position":"pos111","position_recipient":"prr111",
                    "call_token_recipient":"ctr111","write_amount":"100",
                    "gross_premium":"10","fee":"1","net_premium":"9",
                    "range_start":"0","range_end":"100","nonce":"42"}
            }],"nextCursor":"17"}}}"#,
        )
        .unwrap();
        let conn = env.data.unwrap().events;
        assert_eq!(conn.next_cursor.as_deref(), Some("17"));
        let ev = IndexedEvent::try_from(conn.nodes.into_iter().next().unwrap()).unwrap();
        assert_eq!(ev.sequence, 17);
        assert_eq!(ev.slot, 351_234_567);
        assert_eq!(ev.event_type, "WriteExecuted");
        assert_eq!(ev.timestamp_ms, 1_760_000_000_123);
        // Payload is the raw event JSON: solana field names, decimal strings.
        assert_eq!(ev.payload_u64("nonce").unwrap(), 42);
        assert_eq!(ev.payload_str("signer_account").unwrap(), "acc111");
        assert_eq!(ev.payload_str("call_token_recipient").unwrap(), "ctr111");
        assert!(ev.payload_u64("bucket").is_err()); // not a decimal string
    }

    #[test]
    fn decodes_progress_fixture() {
        let p: Progress = serde_json::from_str(
            r#"{"start_slot":351230000,"current_slot":351234567,
                "finalized_slot":351234530,"rate_slots_per_sec":2.4,
                "ms_since_last_slot":410}"#,
        )
        .unwrap();
        assert_eq!(p.finalized_slot, 351_234_530);
        assert_eq!(p.ms_since_last_slot, Some(410));
        // Pre-first-slot form.
        let p: Progress = serde_json::from_str(
            r#"{"start_slot":0,"current_slot":0,"finalized_slot":0,
                "rate_slots_per_sec":0.0,"ms_since_last_slot":null}"#,
        )
        .unwrap();
        assert!(p.ms_since_last_slot.is_none());
    }

    #[test]
    fn graphql_errors_surface() {
        let env: GqlEnvelope<BucketWrap> =
            serde_json::from_str(r#"{"data":null,"errors":[{"message":"boom"}]}"#).unwrap();
        assert!(env.data.is_none());
        assert_eq!(env.errors.unwrap().len(), 1);
    }
}
