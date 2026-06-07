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

use std::collections::BTreeMap;

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
    pub strike: u128,
    pub strike_scale: u8,
    pub expiry_ms: u64,
    pub total_written: u128,
    pub exercise_cursor: u128,
    pub cleaned: bool,
    pub invalidated: bool,
}

/// An account's registered signing key + per-asset balances. `signing_scheme`
/// is `None` only for un-backfilled rows; callers treat that as "unknown
/// signer" (reject), matching the pre-JIT behaviour.
#[derive(Clone, Debug)]
pub struct Account {
    pub account_id: ObjectId,
    pub owner: Option<SuiAddress>,
    pub signing_scheme: Option<SigningScheme>,
    pub signing_pubkey: Vec<u8>,
    pub balances: BTreeMap<AssetType, u64>,
}

impl Account {
    /// On-chain balance for `asset`, 0 if none recorded. Callers subtract
    /// their own local reservations to get spendable balance.
    pub fn balance(&self, asset: &AssetType) -> u64 {
        self.balances.get(asset).copied().unwrap_or(0)
    }
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
    pub premium_received: u64,
    pub mm_account_id: ObjectId,
    pub tx_digest: String,
    pub minted_at_ms: u64,
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
            strikeRaw strikeScale expiryMs totalWrittenRaw exerciseCursorRaw cleaned invalidated}}";
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
            bucketId assetType settlementType strikeRaw strikeScale expiryMs \
            totalWrittenRaw exerciseCursorRaw cleaned invalidated}}";
        let vars = json!({
            "a": active_only,
            "u": asset_type.map(|a| a.as_str()),
            "s": settlement_type.map(|a| a.as_str()),
            "e": expiry_ms.map(|e| e.to_string()),
        });
        let data: BucketsWrap = self.gql(Q, vars).await?;
        data.buckets.into_iter().map(Bucket::try_from).collect()
    }

    /// One account (signing key + balances), or `None` if unknown.
    pub async fn account(&self, account_id: ObjectId) -> Result<Option<Account>> {
        const Q: &str = "query($id:String!){account(id:$id){accountId owner signingScheme \
            signingPubkeyHex balances{assetType balanceRaw}}}";
        let data: AccountWrap = self
            .gql(Q, json!({ "id": account_id.to_hex() }))
            .await?;
        data.account.map(Account::try_from).transpose()
    }

    /// Enriched positions held by `recipient` (mint-time owner-of-record).
    pub async fn positions_by_recipient(&self, recipient: SuiAddress) -> Result<Vec<Position>> {
        const Q: &str = "query($r:String!){positionsByRecipient(recipient:$r){objectId bucketId \
            recipient rangeStartRaw rangeEndRaw assetType settlementType strikeRaw strikeScale \
            expiryMs totalWrittenRaw exerciseCursorRaw premiumReceivedRaw mmAccountId txDigest \
            mintedAtMs}}";
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
            expiryMs totalWrittenRaw exerciseCursorRaw premiumReceivedRaw mmAccountId txDigest \
            mintedAtMs}}";
        let data: PositionsWrap = self.gql(Q, json!({ "ids": object_ids })).await?;
        data.positions.into_iter().map(Position::try_from).collect()
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

    /// All `WriteExecuted` events for `account` with `sequence > after`, in
    /// ascending order. Backs the quoting-service's reservation reconciliation
    /// (the JIT replacement for observing live `WriteExecuted` frames).
    /// Returns `(sequence, nonce)` pairs.
    pub async fn write_executed_for_account_since(
        &self,
        account: ObjectId,
        after: u64,
    ) -> Result<Vec<(u64, u64)>> {
        let filter = json!({
            "eventType": ["WriteExecuted"],
            "payloadContains": { "signer_account_id": account.to_hex() },
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
        let filter = json!({
            "eventType": ["WriteExecuted"],
            "payloadContains": { "call_token_recipient": wallet.to_hex() },
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
        let resp = self
            .http
            .get(&self.progress_url)
            .send()
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
        let resp = self
            .http
            .post(&self.graphql_url)
            .json(&body)
            .send()
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
struct AccountJson {
    account_id: String,
    owner: Option<String>,
    signing_scheme: Option<i32>,
    signing_pubkey_hex: String,
    balances: Vec<BalanceJson>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BalanceJson {
    asset_type: String,
    balance_raw: String,
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
    premium_received_raw: String,
    mm_account_id: String,
    tx_digest: String,
    minted_at_ms: String,
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
        let signing_scheme = a
            .signing_scheme
            .map(|v| SigningScheme::from_u8(parse_u8(v)?).map_err(|e| anyhow!("bad scheme: {e:?}")))
            .transpose()?;
        let mut balances = BTreeMap::new();
        for b in a.balances {
            balances.insert(AssetType::new(b.asset_type), parse_u64(&b.balance_raw)?);
        }
        Ok(Account {
            account_id: parse_object_id(&a.account_id)?,
            owner: a.owner.as_deref().map(parse_address).transpose()?,
            signing_scheme,
            signing_pubkey: decode_hex(&a.signing_pubkey_hex)?,
            balances,
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
            premium_received: parse_u64(&p.premium_received_raw)?,
            mm_account_id: parse_object_id(&p.mm_account_id)?,
            tx_digest: p.tx_digest,
            minted_at_ms: parse_u64(&p.minted_at_ms)?,
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
