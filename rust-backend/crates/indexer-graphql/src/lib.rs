//! Just-in-time client for the indexer's GraphQL query API.
//!
//! Replaces the push-based `indexer-client` (WS fanout → in-memory mirror):
//! consumers call these helpers on demand instead of maintaining a
//! materialized view. Every helper is one HTTP round-trip to the indexer's
//! `/graphql` listener (point lookups) or `/progress` (checkpoint status).
//!
//! On-chain integers cross the wire as decimal strings (the GraphQL API's
//! precision-safe convention); we parse them back into `u64` / `u128` /
//! `u8` here so callers get typed values.

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;

use protocol_types::asset::AssetType;
use protocol_types::events::{ChainEvent, IndexedEvent};
use protocol_types::ids::{ObjectId, SuiAddress};
use protocol_types::SigningScheme;

/// Page size for paginated event scans. The GraphQL `events` query clamps to
/// 1..=1000; we request the max and follow `nextCursor`.
const EVENT_PAGE_LIMIT: i64 = 1000;

/// JIT client over the indexer's GraphQL + progress HTTP API.
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
    pub bucket_id: ObjectId,
    pub asset_type: AssetType,
    pub settlement_type: AssetType,
    /// Fully-qualified type of the per-bucket fungible option coin.
    pub call_type: AssetType,
    pub strike: u128,
    pub strike_scale: u8,
    pub expiry_ms: u64,
    pub total_written: u128,
    pub exercise_cursor: u128,
    pub cleaned: bool,
    pub invalidated: bool,
    /// "call" or "put". Defaults to "call" if the server omits it.
    pub option_kind: String,
    /// DeepBook pool trading this bucket's call coin (SO-152); `None` until
    /// a venue is created.
    pub deepbook_pool_id: Option<ObjectId>,
}

/// A QuoteSigner's registered signing key. Core holds no MM funds anymore
/// (collateral custody lives in per-MM external packages), so there are no
/// balances here. `signing_scheme` is `None` only for un-backfilled rows;
/// callers treat that as "unknown signer" (reject).
#[derive(Clone, Debug)]
pub struct Account {
    pub account_id: ObjectId,
    pub owner: Option<SuiAddress>,
    pub signing_scheme: Option<SigningScheme>,
    pub signing_pubkey: Vec<u8>,
}

/// An enriched position (position row joined to its bucket + mint provenance).
#[derive(Clone, Debug)]
pub struct Position {
    pub object_id: ObjectId,
    pub bucket_id: ObjectId,
    pub recipient: SuiAddress,
    pub range_start: u128,
    pub range_end: u128,
    pub asset_type: AssetType,
    pub settlement_type: AssetType,
    pub strike: u128,
    pub strike_scale: u8,
    pub expiry_ms: u64,
    pub total_written: u128,
    pub exercise_cursor: u128,
    /// "call" or "put". Defaults to "call" if the server omits it.
    pub option_kind: String,
    pub premium_received: u64,
    pub mm_account_id: ObjectId,
    pub tx_digest: String,
    pub minted_at_ms: u64,
}

/// One on-chain auction from the indexer's materialized view (C3, four-
/// package layout). Rows are keyed by the generic auction id.
#[derive(Clone, Debug)]
pub struct Rfq {
    /// The generic auction object id.
    pub rfq_id: ObjectId,
    /// The options_rfq adapter's Rfq metadata object id; `None` for
    /// vault-coupled and swap auctions.
    pub meta_id: Option<ObjectId>,
    /// `None` for swaps / not-yet-enriched coupled auctions.
    pub bucket_id: Option<ObjectId>,
    /// Vault id (coupled auctions) or seller-address-as-id.
    pub origin: ObjectId,
    pub amount: u64,
    pub reserve_premium: u64,
    pub deadline_ms: u64,
    pub best_premium: Option<u64>,
    pub best_bidder: Option<SuiAddress>,
    /// `open` | `settled` | `expired_unsold`.
    pub status: String,
    pub winner: Option<SuiAddress>,
    pub net_premium: Option<u64>,
    pub position_id: Option<ObjectId>,
    /// Premium before the protocol RFQ fee (settled auctions only).
    pub gross_premium: Option<u64>,
    /// Protocol RFQ fee taken at settle (settled auctions only).
    pub fee: Option<u64>,
    /// "call" | "put" | "swap" | "unknown". Defaults to "call" if the
    /// server omits it.
    pub auction_kind: String,
}

/// One bid in an auction's history (C3).
#[derive(Clone, Debug)]
pub struct RfqBid {
    pub rfq_id: ObjectId,
    pub sequence: u64,
    pub bidder: SuiAddress,
    pub call_recipient: SuiAddress,
    pub premium: u64,
}

/// One covered-call vault's headline state (D2).
#[derive(Clone, Debug)]
pub struct Vault {
    pub vault_id: ObjectId,
    pub underlying_type: AssetType,
    pub settlement_type: AssetType,
    pub share_type: AssetType,
    /// Current round (last finalized + 1; 0 = pre-genesis).
    pub round: u64,
    pub current_bucket: Option<ObjectId>,
    pub latest_pps: Option<u128>,
    pub total_shares: u64,
    pub pending_deposits: u64,
    pub deposits_paused: bool,
    /// Active VaultConfig snapshot (consumer-facing subset). `None` until the
    /// config-carrying events are indexed.
    pub mgmt_fee_bps_annual: Option<u64>,
    pub perf_fee_bps: Option<u64>,
    pub round_ms: Option<u64>,
    pub selling_window_ms: Option<u64>,
    pub min_strike_bps_over_spot: Option<u64>,
    pub max_strike_bps_over_spot: Option<u64>,
}

/// One round of a vault's track record (D2). Selection fields land at
/// `select_bucket`; pps/aum/premium at finalize.
#[derive(Clone, Debug)]
pub struct VaultRound {
    pub vault_id: ObjectId,
    pub round: u64,
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

/// One realized-APY point: annualized pps growth landing at a finalized
/// round's finalize time.
#[derive(Clone, Debug)]
pub struct VaultApyPoint {
    pub round: u64,
    pub t_ms: u64,
    pub apy: f64,
}

/// One (vault, owner, round, kind) receipt aggregate (D2).
#[derive(Clone, Debug)]
pub struct VaultReceipt {
    pub vault_id: ObjectId,
    pub owner: SuiAddress,
    pub round: u64,
    /// `deposit` | `withdraw`.
    pub kind: String,
    /// Queued underlying (deposits) / escrowed shares (withdrawals).
    pub amount: u64,
    /// Claimed / completed so far.
    pub settled: u64,
}

/// Checkpoint-ingestion progress (the `/progress` REST endpoint). `Serialize`
/// so a proxying service (api-service Debug page) can re-emit it unchanged.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
pub struct Progress {
    pub start_checkpoint: u64,
    pub current_checkpoint: u64,
    pub tip_checkpoint: Option<u64>,
    pub rate_checkpoints_per_sec: f64,
    pub caught_up: bool,
}

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
    pub async fn bucket(&self, bucket_id: ObjectId) -> Result<Option<Bucket>> {
        const Q: &str = "query($id:String!){bucket(id:$id){bucketId assetType settlementType \
            callType strikeRaw strikeScale expiryMs totalWrittenRaw exerciseCursorRaw cleaned \
            invalidated optionKind deepbookPoolId}}";
        let data: BucketWrap = self
            .gql(Q, json!({ "id": bucket_id.to_hex() }))
            .await?;
        data.bucket.map(Bucket::try_from).transpose()
    }

    /// Buckets matching the filters (all ANDed). `active_only` drops cleaned.
    pub async fn buckets(
        &self,
        active_only: bool,
        asset_type: Option<&AssetType>,
        settlement_type: Option<&AssetType>,
        expiry_ms: Option<u64>,
    ) -> Result<Vec<Bucket>> {
        const Q: &str = "query($a:Boolean,$u:String,$s:String,$e:String){\
            buckets(activeOnly:$a,assetType:$u,settlementType:$s,expiryMs:$e){\
            bucketId assetType settlementType callType strikeRaw strikeScale expiryMs \
            totalWrittenRaw exerciseCursorRaw cleaned invalidated optionKind deepbookPoolId}}";
        let vars = json!({
            "a": active_only,
            "u": asset_type.map(|a| a.as_str()),
            "s": settlement_type.map(|a| a.as_str()),
            "e": expiry_ms.map(|e| e.to_string()),
        });
        let data: BucketsWrap = self.gql(Q, vars).await?;
        data.buckets.into_iter().map(Bucket::try_from).collect()
    }

    /// One QuoteSigner (registered signing key), or `None` if unknown.
    pub async fn account(&self, account_id: ObjectId) -> Result<Option<Account>> {
        const Q: &str = "query($id:String!){account(id:$id){accountId owner signingScheme \
            signingPubkeyHex}}";
        let data: AccountWrap = self
            .gql(Q, json!({ "id": account_id.to_hex() }))
            .await?;
        data.account.map(Account::try_from).transpose()
    }

    /// Enriched positions held by `recipient` (mint-time owner-of-record).
    pub async fn positions_by_recipient(&self, recipient: SuiAddress) -> Result<Vec<Position>> {
        const Q: &str = "query($r:String!){positionsByRecipient(recipient:$r){objectId bucketId \
            recipient rangeStartRaw rangeEndRaw assetType settlementType strikeRaw strikeScale \
            expiryMs totalWrittenRaw exerciseCursorRaw optionKind premiumReceivedRaw mmAccountId \
            txDigest mintedAtMs}}";
        let data: PositionsByRecipientWrap =
            self.gql(Q, json!({ "r": recipient.to_hex() })).await?;
        data.positions_by_recipient
            .into_iter()
            .map(Position::try_from)
            .collect()
    }

    /// Enriched positions for a set of on-chain `Position` object ids. Unknown
    /// ids are simply absent from the result.
    pub async fn positions_by_object_ids(&self, object_ids: &[String]) -> Result<Vec<Position>> {
        if object_ids.is_empty() {
            return Ok(vec![]);
        }
        const Q: &str = "query($ids:[String!]!){positions(objectIds:$ids){objectId bucketId \
            recipient rangeStartRaw rangeEndRaw assetType settlementType strikeRaw strikeScale \
            expiryMs totalWrittenRaw exerciseCursorRaw optionKind premiumReceivedRaw mmAccountId \
            txDigest mintedAtMs}}";
        let data: PositionsWrap = self.gql(Q, json!({ "ids": object_ids })).await?;
        data.positions.into_iter().map(Position::try_from).collect()
    }

    // ── rfq / vault views (C3 / D2) ───────────────────────────────────────

    /// RFQ auctions, optionally filtered by status
    /// (`open` | `settled` | `expired_unsold`) and/or origin (vault id).
    pub async fn rfqs(
        &self,
        status: Option<&str>,
        origin: Option<ObjectId>,
    ) -> Result<Vec<Rfq>> {
        const Q: &str = "query($s:String,$o:String){rfqs(status:$s,origin:$o){rfqId metaId \
            bucketId origin amountRaw reservePremiumRaw deadlineMs bestPremiumRaw bestBidder \
            status winner netPremiumRaw positionId grossPremiumRaw feeRaw auctionKind}}";
        let vars = json!({ "s": status, "o": origin.map(|o| o.to_hex()) });
        let data: RfqsWrap = self.gql(Q, vars).await?;
        data.rfqs.into_iter().map(Rfq::try_from).collect()
    }

    /// Bid history for one auction, ascending.
    pub async fn rfq_bids(&self, rfq_id: ObjectId) -> Result<Vec<RfqBid>> {
        const Q: &str = "query($id:String!){rfqBids(rfqId:$id){rfqId sequence bidder \
            callRecipient premiumRaw}}";
        let data: RfqBidsWrap = self.gql(Q, json!({ "id": rfq_id.to_hex() })).await?;
        data.rfq_bids.into_iter().map(RfqBid::try_from).collect()
    }

    /// All covered-call vaults.
    pub async fn vaults(&self) -> Result<Vec<Vault>> {
        const Q: &str = "query{vaults{vaultId underlyingType settlementType shareType round \
            currentBucket latestPpsRaw totalSharesRaw pendingDepositsRaw depositsPaused \
            mgmtFeeBpsAnnual perfFeeBps roundMs sellingWindowMs minStrikeBpsOverSpot \
            maxStrikeBpsOverSpot}}";
        let data: VaultsWrap = self.gql(Q, json!({})).await?;
        data.vaults.into_iter().map(Vault::try_from).collect()
    }

    /// One vault by id, or `None` if unknown.
    pub async fn vault(&self, vault_id: ObjectId) -> Result<Option<Vault>> {
        const Q: &str = "query($id:String!){vault(id:$id){vaultId underlyingType settlementType \
            shareType round currentBucket latestPpsRaw totalSharesRaw pendingDepositsRaw \
            depositsPaused mgmtFeeBpsAnnual perfFeeBps roundMs sellingWindowMs \
            minStrikeBpsOverSpot maxStrikeBpsOverSpot}}";
        let data: VaultWrap = self.gql(Q, json!({ "id": vault_id.to_hex() })).await?;
        data.vault.map(Vault::try_from).transpose()
    }

    /// One vault's round history, ascending (the track record).
    pub async fn vault_rounds(&self, vault_id: ObjectId) -> Result<Vec<VaultRound>> {
        const Q: &str = "query($id:String!){vaultRounds(vaultId:$id){vaultId round bucketId \
            strikeRaw strikeScale expiryMs ppsRaw aumRaw sharesRaw premiumCollectedRaw \
            mgmtFeeRaw perfFeeRaw finalizedAtMs}}";
        let data: VaultRoundsWrap = self.gql(Q, json!({ "id": vault_id.to_hex() })).await?;
        data.vault_rounds.into_iter().map(VaultRound::try_from).collect()
    }

    /// One vault's realized-APY series (annualized pps growth per finalized
    /// round), computed indexer-side from chain data.
    pub async fn vault_apy(&self, vault_id: ObjectId) -> Result<Vec<VaultApyPoint>> {
        const Q: &str = "query($id:String!){vaultApy(vaultId:$id){round tMs apy}}";
        let data: VaultApyWrap = self.gql(Q, json!({ "id": vault_id.to_hex() })).await?;
        data.vault_apy.into_iter().map(VaultApyPoint::try_from).collect()
    }

    /// Receipt aggregates for one vault, optionally scoped to an owner.
    pub async fn vault_receipts(
        &self,
        vault_id: ObjectId,
        owner: Option<SuiAddress>,
    ) -> Result<Vec<VaultReceipt>> {
        const Q: &str = "query($id:String!,$o:String){vaultReceipts(vaultId:$id,owner:$o){\
            vaultId owner round kind amountRaw settledRaw}}";
        let vars = json!({ "id": vault_id.to_hex(), "o": owner.map(|o| o.to_hex()) });
        let data: VaultReceiptsWrap = self.gql(Q, vars).await?;
        data.vault_receipts
            .into_iter()
            .map(VaultReceipt::try_from)
            .collect()
    }

    // ── event-log scans ───────────────────────────────────────────────────

    /// Highest persisted sequence (0 if the log is empty). This is the JIT
    /// equivalent of the fanout `last_sequence` high-water mark — used by the
    /// option-scheduler reconciler to tell whether the indexer has caught up.
    pub async fn head_sequence(&self) -> Result<u64> {
        const Q: &str =
            "query{events(order:SEQUENCE_DESC,limit:1){nodes{sequence timestampMs payload}}}";
        let data: EventsWrap = self.gql(Q, json!({})).await?;
        Ok(data
            .events
            .nodes
            .first()
            .map(|n| n.sequence.parse::<u64>().unwrap_or(0))
            .unwrap_or(0))
    }

    /// All `WriteExecuted` events for the QuoteSigner `account` with
    /// `sequence > after`, in ascending order. Backs the quoting-service's
    /// reputation fill accounting (the JIT replacement for observing live
    /// `WriteExecuted` frames). Returns `(sequence, nonce)` pairs.
    pub async fn write_executed_for_account_since(
        &self,
        account: ObjectId,
        after: u64,
    ) -> Result<Vec<(u64, u64)>> {
        // The stored payload is the tagged `ChainEvent` envelope
        // (`{"type":…,"payload":{…}}`), so the field match must nest under
        // `payload` for JSONB `@>` to hit.
        let filter = json!({
            "eventType": ["WriteExecuted"],
            "payloadContains": { "payload": { "signer_id": account.to_hex() } },
        });
        let events = self.scan_events(filter, after).await?;
        let mut out = Vec::with_capacity(events.len());
        for ev in events {
            if let ChainEvent::WriteExecuted(w) = ev.event {
                out.push((ev.sequence, w.nonce));
            }
        }
        Ok(out)
    }

    /// All `WriteExecuted` events whose `call_token_recipient` is `wallet`, in
    /// ascending order. Backs the api-service call-token "lot" provenance list.
    pub async fn write_executed_for_recipient(
        &self,
        wallet: SuiAddress,
    ) -> Result<Vec<IndexedEvent>> {
        // Match nests under `payload` — the column stores the tagged
        // `ChainEvent` envelope (`{"type":…,"payload":{…}}`), not the bare
        // event fields. See `write_executed_for_account_since`.
        let filter = json!({
            "eventType": ["WriteExecuted"],
            "payloadContains": { "payload": { "call_token_recipient": wallet.to_hex() } },
        });
        self.scan_events(filter, 0).await
    }

    /// All `PutWriteExecuted` events for `account` with `sequence > after`, in
    /// ascending order. The put-side mirror of
    /// [`write_executed_for_account_since`]. Returns `(sequence, nonce)` pairs.
    pub async fn put_write_executed_for_account_since(
        &self,
        account: ObjectId,
        after: u64,
    ) -> Result<Vec<(u64, u64)>> {
        let filter = json!({
            "eventType": ["PutWriteExecuted"],
            "payloadContains": { "payload": { "signer_id": account.to_hex() } },
        });
        let events = self.scan_events(filter, after).await?;
        let mut out = Vec::with_capacity(events.len());
        for ev in events {
            if let ChainEvent::PutWriteExecuted(w) = ev.event {
                out.push((ev.sequence, w.nonce));
            }
        }
        Ok(out)
    }

    /// All `PutWriteExecuted` events whose `put_token_recipient` is `wallet`, in
    /// ascending order. The put-side mirror of
    /// [`write_executed_for_recipient`].
    pub async fn put_write_executed_for_recipient(
        &self,
        wallet: SuiAddress,
    ) -> Result<Vec<IndexedEvent>> {
        let filter = json!({
            "eventType": ["PutWriteExecuted"],
            "payloadContains": { "payload": { "put_token_recipient": wallet.to_hex() } },
        });
        self.scan_events(filter, 0).await
    }

    /// Every event `wallet` participated in (any role), ascending by sequence.
    /// The indexer's `participant` filter matches the per-event address fan-out
    /// — including the account owner for deposit/withdraw — so this is the
    /// single query the activity feed needs. Backs api-service `/events`.
    pub async fn events_for_participant(
        &self,
        wallet: SuiAddress,
    ) -> Result<Vec<IndexedEvent>> {
        let filter = json!({ "participant": wallet.to_hex() });
        self.scan_events(filter, 0).await
    }

    /// Every `DeepBookOrderFilled` touching BalanceManager `bm` (as taker or
    /// maker BM), ascending by sequence (SO-209). A BM id only ever shows up as
    /// a fill participant, so the participant filter alone yields exactly the
    /// fills — the api-service maps a wallet's BM id to this to attribute
    /// DeepBook cost basis without a BM→owner table.
    pub async fn deepbook_fills_for_bm(&self, bm: ObjectId) -> Result<Vec<IndexedEvent>> {
        let filter = json!({ "participant": bm.to_hex() });
        self.scan_events(filter, 0).await
    }

    /// Paginate the `events` query (ascending) for `filter`, starting after
    /// `after`, decoding each node's payload into a typed `IndexedEvent`.
    async fn scan_events(
        &self,
        filter: serde_json::Value,
        after: u64,
    ) -> Result<Vec<IndexedEvent>> {
        const Q: &str = "query($f:EventFilterInput,$limit:Int,$after:String){\
            events(filter:$f,order:SEQUENCE_ASC,limit:$limit,after:$after){\
            nodes{sequence timestampMs payload} nextCursor}}";
        let mut cursor: Option<String> = if after == 0 {
            None
        } else {
            Some(after.to_string())
        };
        let mut out = Vec::new();
        loop {
            let vars = json!({ "f": filter, "limit": EVENT_PAGE_LIMIT, "after": cursor });
            let data: EventsWrap = self.gql(Q, vars).await?;
            for node in data.events.nodes {
                out.push(node.into_indexed_event()?);
            }
            match data.events.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok(out)
    }

    // ── progress ──────────────────────────────────────────────────────────

    /// Checkpoint-ingestion progress (`GET /progress`).
    pub async fn progress(&self) -> Result<Progress> {
        let resp = observability::client::instrumented("indexer", "GET /progress", |h| {
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
        let resp = observability::client::instrumented("indexer", "POST /graphql", |h| {
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
                bail!("indexer graphql errors: {errors:?}");
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
struct EventsWrap {
    events: EventConnectionJson,
}
#[derive(Deserialize)]
struct RfqsWrap {
    rfqs: Vec<RfqJson>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RfqBidsWrap {
    rfq_bids: Vec<RfqBidJson>,
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
struct VaultReceiptsWrap {
    vault_receipts: Vec<VaultReceiptJson>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VaultApyWrap {
    vault_apy: Vec<VaultApyJson>,
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
    timestamp_ms: String,
    payload: serde_json::Value,
}

impl EventNodeJson {
    fn into_indexed_event(self) -> Result<IndexedEvent> {
        let event: ChainEvent = serde_json::from_value(self.payload)
            .context("decoding event payload into ChainEvent")?;
        Ok(IndexedEvent {
            sequence: self.sequence.parse().context("parsing event sequence")?,
            timestamp_ms: self.timestamp_ms.parse().context("parsing event timestamp")?,
            event,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BucketJson {
    bucket_id: String,
    asset_type: String,
    settlement_type: String,
    call_type: String,
    strike_raw: String,
    strike_scale: i32,
    expiry_ms: String,
    total_written_raw: String,
    exercise_cursor_raw: String,
    cleaned: bool,
    invalidated: bool,
    #[serde(default = "default_option_kind")]
    option_kind: String,
    #[serde(default)]
    deepbook_pool_id: Option<String>,
}

/// Serde default for `option_kind` — calls are the historical default, so a
/// server that omits the field is treated as a call.
fn default_option_kind() -> String {
    "call".to_string()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountJson {
    account_id: String,
    owner: Option<String>,
    signing_scheme: Option<i32>,
    signing_pubkey_hex: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PositionJson {
    object_id: String,
    bucket_id: String,
    recipient: String,
    range_start_raw: String,
    range_end_raw: String,
    asset_type: String,
    settlement_type: String,
    strike_raw: String,
    strike_scale: i32,
    expiry_ms: String,
    total_written_raw: String,
    exercise_cursor_raw: String,
    #[serde(default = "default_option_kind")]
    option_kind: String,
    premium_received_raw: String,
    mm_account_id: String,
    tx_digest: String,
    minted_at_ms: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RfqJson {
    rfq_id: String,
    #[serde(default)]
    meta_id: Option<String>,
    #[serde(default)]
    bucket_id: Option<String>,
    origin: String,
    amount_raw: String,
    reserve_premium_raw: String,
    deadline_ms: String,
    best_premium_raw: Option<String>,
    best_bidder: Option<String>,
    status: String,
    winner: Option<String>,
    net_premium_raw: Option<String>,
    position_id: Option<String>,
    #[serde(default)]
    gross_premium_raw: Option<String>,
    #[serde(default)]
    fee_raw: Option<String>,
    #[serde(default = "default_option_kind")]
    auction_kind: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RfqBidJson {
    rfq_id: String,
    sequence: String,
    bidder: String,
    call_recipient: String,
    premium_raw: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VaultJson {
    vault_id: String,
    underlying_type: String,
    settlement_type: String,
    share_type: String,
    round: String,
    current_bucket: Option<String>,
    latest_pps_raw: Option<String>,
    total_shares_raw: String,
    pending_deposits_raw: String,
    deposits_paused: bool,
    #[serde(default)]
    mgmt_fee_bps_annual: Option<String>,
    #[serde(default)]
    perf_fee_bps: Option<String>,
    #[serde(default)]
    round_ms: Option<String>,
    #[serde(default)]
    selling_window_ms: Option<String>,
    #[serde(default)]
    min_strike_bps_over_spot: Option<String>,
    #[serde(default)]
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
struct VaultReceiptJson {
    vault_id: String,
    owner: String,
    round: String,
    kind: String,
    amount_raw: String,
    settled_raw: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VaultApyJson {
    round: String,
    t_ms: String,
    apy: f64,
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

// ── parsing wire → domain ──────────────────────────────────────────────────

fn parse_object_id(s: &str) -> Result<ObjectId> {
    ObjectId::from_hex(s).map_err(|e| anyhow!("bad object id {s}: {e}"))
}
fn parse_address(s: &str) -> Result<SuiAddress> {
    SuiAddress::from_hex(s).map_err(|e| anyhow!("bad address {s}: {e}"))
}
fn parse_u128(s: &str) -> Result<u128> {
    s.parse().map_err(|e| anyhow!("bad u128 {s:?}: {e}"))
}
fn parse_u64(s: &str) -> Result<u64> {
    s.parse().map_err(|e| anyhow!("bad u64 {s:?}: {e}"))
}
fn parse_u8(v: i32) -> Result<u8> {
    u8::try_from(v).map_err(|_| anyhow!("value {v} out of u8 range"))
}

impl TryFrom<BucketJson> for Bucket {
    type Error = anyhow::Error;
    fn try_from(b: BucketJson) -> Result<Self> {
        Ok(Bucket {
            bucket_id: parse_object_id(&b.bucket_id)?,
            asset_type: AssetType::new(b.asset_type),
            settlement_type: AssetType::new(b.settlement_type),
            call_type: AssetType::new(b.call_type),
            strike: parse_u128(&b.strike_raw)?,
            strike_scale: parse_u8(b.strike_scale)?,
            expiry_ms: parse_u64(&b.expiry_ms)?,
            total_written: parse_u128(&b.total_written_raw)?,
            exercise_cursor: parse_u128(&b.exercise_cursor_raw)?,
            cleaned: b.cleaned,
            invalidated: b.invalidated,
            option_kind: b.option_kind,
            deepbook_pool_id: b
                .deepbook_pool_id
                .as_deref()
                .map(parse_object_id)
                .transpose()?,
        })
    }
}

impl TryFrom<AccountJson> for Account {
    type Error = anyhow::Error;
    fn try_from(a: AccountJson) -> Result<Self> {
        let signing_scheme = a
            .signing_scheme
            .map(|v| SigningScheme::from_u8(parse_u8(v)?).map_err(|e| anyhow!("bad scheme: {e:?}")))
            .transpose()?;
        Ok(Account {
            account_id: parse_object_id(&a.account_id)?,
            owner: a.owner.as_deref().map(parse_address).transpose()?,
            signing_scheme,
            signing_pubkey: decode_hex(&a.signing_pubkey_hex)?,
        })
    }
}

impl TryFrom<PositionJson> for Position {
    type Error = anyhow::Error;
    fn try_from(p: PositionJson) -> Result<Self> {
        Ok(Position {
            object_id: parse_object_id(&p.object_id)?,
            bucket_id: parse_object_id(&p.bucket_id)?,
            recipient: parse_address(&p.recipient)?,
            range_start: parse_u128(&p.range_start_raw)?,
            range_end: parse_u128(&p.range_end_raw)?,
            asset_type: AssetType::new(p.asset_type),
            settlement_type: AssetType::new(p.settlement_type),
            strike: parse_u128(&p.strike_raw)?,
            strike_scale: parse_u8(p.strike_scale)?,
            expiry_ms: parse_u64(&p.expiry_ms)?,
            total_written: parse_u128(&p.total_written_raw)?,
            exercise_cursor: parse_u128(&p.exercise_cursor_raw)?,
            option_kind: p.option_kind,
            premium_received: parse_u64(&p.premium_received_raw)?,
            mm_account_id: parse_object_id(&p.mm_account_id)?,
            tx_digest: p.tx_digest,
            minted_at_ms: parse_u64(&p.minted_at_ms)?,
        })
    }
}

impl TryFrom<RfqJson> for Rfq {
    type Error = anyhow::Error;
    fn try_from(r: RfqJson) -> Result<Self> {
        Ok(Rfq {
            rfq_id: parse_object_id(&r.rfq_id)?,
            meta_id: r.meta_id.as_deref().map(parse_object_id).transpose()?,
            bucket_id: r.bucket_id.as_deref().map(parse_object_id).transpose()?,
            origin: parse_object_id(&r.origin)?,
            amount: parse_u64(&r.amount_raw)?,
            reserve_premium: parse_u64(&r.reserve_premium_raw)?,
            deadline_ms: parse_u64(&r.deadline_ms)?,
            best_premium: r.best_premium_raw.as_deref().map(parse_u64).transpose()?,
            best_bidder: r.best_bidder.as_deref().map(parse_address).transpose()?,
            status: r.status,
            winner: r.winner.as_deref().map(parse_address).transpose()?,
            net_premium: r.net_premium_raw.as_deref().map(parse_u64).transpose()?,
            position_id: r.position_id.as_deref().map(parse_object_id).transpose()?,
            gross_premium: r.gross_premium_raw.as_deref().map(parse_u64).transpose()?,
            fee: r.fee_raw.as_deref().map(parse_u64).transpose()?,
            auction_kind: r.auction_kind,
        })
    }
}

impl TryFrom<RfqBidJson> for RfqBid {
    type Error = anyhow::Error;
    fn try_from(b: RfqBidJson) -> Result<Self> {
        Ok(RfqBid {
            rfq_id: parse_object_id(&b.rfq_id)?,
            sequence: parse_u64(&b.sequence)?,
            bidder: parse_address(&b.bidder)?,
            call_recipient: parse_address(&b.call_recipient)?,
            premium: parse_u64(&b.premium_raw)?,
        })
    }
}

impl TryFrom<VaultJson> for Vault {
    type Error = anyhow::Error;
    fn try_from(v: VaultJson) -> Result<Self> {
        Ok(Vault {
            vault_id: parse_object_id(&v.vault_id)?,
            underlying_type: AssetType::new(v.underlying_type),
            settlement_type: AssetType::new(v.settlement_type),
            share_type: AssetType::new(v.share_type),
            round: parse_u64(&v.round)?,
            current_bucket: v.current_bucket.as_deref().map(parse_object_id).transpose()?,
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
            vault_id: parse_object_id(&r.vault_id)?,
            round: parse_u64(&r.round)?,
            bucket_id: r.bucket_id.as_deref().map(parse_object_id).transpose()?,
            strike: r.strike_raw.as_deref().map(parse_u128).transpose()?,
            strike_scale: r.strike_scale.map(parse_u8).transpose()?,
            expiry_ms: r.expiry_ms.as_deref().map(parse_u64).transpose()?,
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

impl TryFrom<VaultReceiptJson> for VaultReceipt {
    type Error = anyhow::Error;
    fn try_from(r: VaultReceiptJson) -> Result<Self> {
        Ok(VaultReceipt {
            vault_id: parse_object_id(&r.vault_id)?,
            owner: parse_address(&r.owner)?,
            round: parse_u64(&r.round)?,
            kind: r.kind,
            amount: parse_u64(&r.amount_raw)?,
            settled: parse_u64(&r.settled_raw)?,
        })
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let bytes = vec![0x00u8, 0x0f, 0xab, 0xff];
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(decode_hex(&hex).unwrap(), bytes);
        assert_eq!(decode_hex("0x0f").unwrap(), vec![0x0f]);
    }

    #[test]
    fn progress_url_derived_from_graphql_url() {
        let c = IndexerClient::new("http://indexer:9002/graphql".to_string());
        assert_eq!(c.progress_url, "http://indexer:9002/progress");
    }
}
