//! Client for **Crossbar**, Switchboard's utility server (SO-335).
//!
//! Crossbar is what Hermes is to Pyth: the off-chain endpoint that turns
//! feed identifiers into signed oracle data we can submit on chain. We
//! run our own (`switchboardlabs/rust-crossbar`, added in SO-333 and
//! reachable at `/{env}/crossbar/`) rather than the public instance,
//! which is rate-limited.
//!
//! ## The endpoint
//!
//! [`CrossbarClient::fetch_quotes`] calls
//! `GET /v2/update/{comma-separated feed hashes}` — "build a chain-specific
//! consensus payload for one or more v2 feed hashes". The response shape
//! below was captured from a live call against
//! `crossbar.switchboard.xyz`, not inferred from docs:
//!
//! ```json
//! {
//!   "medianResponses": [{"value":"63456010000000000000000","feedHash":"4cd1…","numOracles":1}],
//!   "oracleResponses": [{
//!     "oraclePubkey":"405a…", "signature":"<base64>", "recoveryId":1,
//!     "feedResponses":[{"feed_hash":"4cd1…","min_oracle_samples":1,"queue_pubkey":"8680…"}]
//!   }],
//!   "timestamp": 1785700471, "slot": 42, "recentHash": "…"
//! }
//! ```
//!
//! Two shape details that are easy to get wrong:
//!
//! - **Signatures are base64**, not hex — the on-chain `run_N` wants raw
//!   bytes, so they are decoded here.
//! - **`oraclePubkey` is a Switchboard key, not a Sui object id.** The
//!   Move call takes `&Oracle` *objects*, so the pubkeys have to be
//!   resolved through `GET /oracles/sui`, which returns
//!   `{oracle_id, oracle_key}` pairs. Assuming the response already held
//!   object ids would produce a PTB that cannot be built.
//!
//! ## Feed hashes are created, not looked up
//!
//! Unlike a Pyth feed id, a Switchboard feed hash is the **content hash of
//! a job definition** (`GET /fetch/{hash}` returns the job graph that
//! produced it). Canonical symbol → hash pairs come from
//! `GET /stream/surge_feeds`; bespoke feeds are created with
//! `POST /v2/store`. See `docs/oracle-abstraction-plan.md`.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use serde::Deserialize;
use sui_types::base_types::ObjectID;
use tracing::debug;

/// Largest `run_N` arity `switchboard::quote_submit_result_action`
/// exposes. More signatures than that cannot be submitted on chain.
pub const MAX_ORACLES: usize = 6;

/// Signed oracle data for one or more feeds, ready for on-chain submit.
///
/// Field-for-field what `quote_submit_result_action::run_N` consumes;
/// `sui_tx::tx::oracle::switchboard` lays it straight into the PTB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteBundle {
    pub feed_ids: Vec<Vec<u8>>,
    pub values: Vec<u128>,
    pub values_neg: Vec<bool>,
    pub min_oracle_samples: Vec<u8>,
    pub signatures: Vec<Vec<u8>>,
    pub slot: u64,
    pub timestamp_seconds: u64,
    /// Sui `Oracle` OBJECT ids, resolved from the response's pubkeys.
    pub oracle_ids: Vec<ObjectID>,
    /// The queue these signatures were produced under, as reported by
    /// Crossbar (`queue_pubkey`, 32 bytes hex, no `0x`).
    ///
    /// Carried so callers can check it against the Sui `Queue` they are
    /// about to submit to — see [`QuoteBundle::require_queue`].
    pub queue_key: String,
}

impl QuoteBundle {
    /// Refuse a bundle signed under a different queue than the one we
    /// will submit against.
    ///
    /// This is not theoretical. Switchboard runs one queue per
    /// network-ish domain, and `run_N` validates every oracle against
    /// the `&Queue` object passed in, so a cross-queue bundle aborts on
    /// chain with nothing useful in the error. Observed concretely:
    /// the PUBLIC `crossbar.switchboard.xyz` answers for queue
    /// `86807068…` while Sui testnet's on-chain oracle queue is
    /// `c9477bfb…`. Catching it here turns a confusing revert into a
    /// clear off-chain message.
    pub fn require_queue(&self, expected_queue_key: &str) -> Result<()> {
        let want = strip0x(expected_queue_key).to_ascii_lowercase();
        if self.queue_key == want {
            return Ok(());
        }
        Err(anyhow!(
            "crossbar signed these quotes under queue {} but this network's \
             Switchboard queue is {want} — the on-chain run_N would reject \
             every signature. Point crossbar at the RPC backing this \
             network's queue.",
            self.queue_key
        ))
    }
}

// ── wire types (mirror the live response above) ──────────────────────

#[derive(Debug, Deserialize)]
struct MedianResponse {
    /// 18-decimal fixed point as a decimal string; a $63,456 price is
    /// 6.3456e22, far past `u64`, so it is never a JSON number.
    value: String,
    #[serde(rename = "feedHash")]
    feed_hash: String,
}

#[derive(Debug, Deserialize)]
struct FeedResponse {
    feed_hash: String,
    #[serde(default = "one")]
    min_oracle_samples: u8,
    /// 32-byte hex. Same for every feed in a bundle (one queue signs it).
    #[serde(default)]
    queue_pubkey: Option<String>,
}

fn one() -> u8 {
    1
}

#[derive(Debug, Deserialize)]
struct OracleResponse {
    #[serde(rename = "oraclePubkey")]
    oracle_pubkey: String,
    /// base64.
    signature: String,
    #[serde(rename = "feedResponses", default)]
    feed_responses: Vec<FeedResponse>,
}

#[derive(Debug, Deserialize)]
struct UpdateResponse {
    #[serde(rename = "medianResponses")]
    median_responses: Vec<MedianResponse>,
    #[serde(rename = "oracleResponses")]
    oracle_responses: Vec<OracleResponse>,
    timestamp: u64,
    slot: u64,
}

/// `GET /oracles/sui` row: the pubkey ↔ Sui object mapping.
#[derive(Debug, Clone, Deserialize)]
pub struct SuiOracle {
    pub oracle_id: String,
    pub oracle_key: String,
}

fn strip0x(s: &str) -> &str {
    s.trim().trim_start_matches("0x").trim_start_matches("0X")
}

impl UpdateResponse {
    /// Fold the response into a submit-ready bundle.
    ///
    /// `oracle_objects` maps `oracle_key` → Sui object id (from
    /// [`CrossbarClient::sui_oracles`]). An unmapped signer is a hard
    /// error: dropping it would silently change the consensus set the
    /// on-chain verifier checks.
    fn into_bundle(self, oracle_objects: &BTreeMap<String, ObjectID>) -> Result<QuoteBundle> {
        if self.median_responses.is_empty() {
            return Err(anyhow!("crossbar returned no median responses"));
        }
        if self.oracle_responses.is_empty() {
            return Err(anyhow!("crossbar returned no oracle responses"));
        }

        let mut feed_ids = Vec::with_capacity(self.median_responses.len());
        let mut values = Vec::with_capacity(self.median_responses.len());
        let mut values_neg = Vec::with_capacity(self.median_responses.len());
        for m in &self.median_responses {
            let bytes = hex::decode(strip0x(&m.feed_hash)).context("decoding feed hash")?;
            if bytes.len() != 32 {
                return Err(anyhow!(
                    "feed hash {} is {} bytes; expected 32",
                    m.feed_hash,
                    bytes.len()
                ));
            }
            feed_ids.push(bytes);
            let raw = m.value.trim();
            // Defensive: prices are non-negative here, but the on-chain
            // type carries a sign bit, so honour one if it ever appears
            // rather than parsing it as garbage.
            let (neg, digits) = match raw.strip_prefix('-') {
                Some(rest) => (true, rest),
                None => (false, raw),
            };
            values.push(digits.parse::<u128>().with_context(|| {
                format!("parsing 18-decimal value {raw:?} for feed {}", m.feed_hash)
            })?);
            values_neg.push(neg);
        }

        // `min_oracle_samples` is per feed, reported inside each oracle's
        // feed responses. Take the max across oracles so the on-chain
        // check is never looser than any signer intended.
        let mut min_samples: BTreeMap<String, u8> = BTreeMap::new();
        for o in &self.oracle_responses {
            for fr in &o.feed_responses {
                let k = strip0x(&fr.feed_hash).to_ascii_lowercase();
                let e = min_samples.entry(k).or_insert(fr.min_oracle_samples);
                *e = (*e).max(fr.min_oracle_samples);
            }
        }
        let min_oracle_samples = self
            .median_responses
            .iter()
            .map(|m| {
                *min_samples
                    .get(&strip0x(&m.feed_hash).to_ascii_lowercase())
                    .unwrap_or(&1)
            })
            .collect();

        let mut signatures = Vec::with_capacity(self.oracle_responses.len());
        let mut oracle_ids = Vec::with_capacity(self.oracle_responses.len());
        for o in &self.oracle_responses {
            signatures.push(
                base64::engine::general_purpose::STANDARD
                    .decode(o.signature.trim())
                    .context("decoding base64 oracle signature")?,
            );
            let key = strip0x(&o.oracle_pubkey).to_ascii_lowercase();
            let id = oracle_objects.get(&key).ok_or_else(|| {
                anyhow!(
                    "oracle {key} signed the bundle but has no Sui object in /oracles/sui — \
                     cannot build the on-chain call"
                )
            })?;
            oracle_ids.push(*id);
        }

        if oracle_ids.len() > MAX_ORACLES {
            return Err(anyhow!(
                "crossbar returned {} oracle signatures; the on-chain package exposes \
                 run_1..run_{MAX_ORACLES} — request fewer",
                oracle_ids.len()
            ));
        }

        // Every feed response in a bundle carries the same queue; take
        // the first and refuse a bundle that reports none, since an
        // unverifiable queue is exactly what `require_queue` exists for.
        let queue_key = self
            .oracle_responses
            .iter()
            .flat_map(|o| o.feed_responses.iter())
            .find_map(|fr| fr.queue_pubkey.as_deref())
            .map(|q| strip0x(q).to_ascii_lowercase())
            .ok_or_else(|| {
                anyhow!("crossbar response carries no queue_pubkey — cannot verify the queue")
            })?;

        Ok(QuoteBundle {
            feed_ids,
            values,
            values_neg,
            min_oracle_samples,
            signatures,
            slot: self.slot,
            timestamp_seconds: self.timestamp,
            oracle_ids,
            queue_key,
        })
    }
}

/// Guard clauses for [`CrossbarClient::fetch_quotes`], split out so they
/// are testable without a live Crossbar.
fn validate_request(feed_hashes: &[String]) -> Result<()> {
    if feed_hashes.is_empty() {
        return Err(anyhow!("fetch_quotes called with no feed hashes"));
    }
    for h in feed_hashes {
        let bytes =
            hex::decode(strip0x(h)).with_context(|| format!("feed hash {h:?} is not hex"))?;
        if bytes.len() != 32 {
            return Err(anyhow!(
                "feed hash {h:?} is {} bytes; Switchboard feed hashes are 32",
                bytes.len()
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct CrossbarClient {
    base_url: String,
    /// `network` query value appended to quote requests (SO-346).
    ///
    /// Crossbar's Solana anchoring is per-cluster and the DEFAULT is
    /// mainnet: an unparameterized `/v2/update` returns bundles signed
    /// under the MAINNET queue (`86807068…`), which Sui testnet's
    /// on-chain queue (`c9477bfb…`) rejects wholesale. Verified live:
    /// `?network=devnet` flips the signing set to the testnet queue.
    /// `None` keeps the instance default (mainnet).
    network: Option<String>,
    http: reqwest::Client,
}

impl CrossbarClient {
    pub fn new(base_url: impl Into<String>, network: Option<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            network,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .expect("building crossbar http client"),
        }
    }

    /// `?network=…` suffix for quote-shaping endpoints, empty when unset.
    fn network_query(&self) -> String {
        match &self.network {
            Some(n) => format!("?network={n}"),
            None => String::new(),
        }
    }

    /// Liveness. Crossbar is a third-party image we run unmodified, so a
    /// boot-time probe distinguishes "misconfigured" from "not up yet".
    pub async fn health(&self) -> Result<()> {
        let url = format!("{}/health", self.base_url);
        self.http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("GET {url}"))?;
        Ok(())
    }

    /// `GET /oracles/sui` — the Switchboard-pubkey → Sui-object mapping
    /// the quote payload needs. Cheap and slow-changing; resolve once at
    /// boot rather than per quote.
    /// CAUTION (SO-346): with `network=devnet` the public instance lists
    /// Sui DEVNET object ids, which do not exist on testnet. For Sui
    /// testnet use [`oracles_from_queue`] instead — the chain is
    /// authoritative and always names objects that actually exist.
    pub async fn sui_oracles(&self) -> Result<BTreeMap<String, ObjectID>> {
        let url = format!("{}/oracles/sui{}", self.base_url, self.network_query());
        let rows: Vec<SuiOracle> = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("GET {url}"))?
            .json()
            .await
            .context("decoding /oracles/sui")?;
        rows.into_iter()
            .map(|r| {
                let id = ObjectID::from_hex_literal(r.oracle_id.trim())
                    .with_context(|| format!("parsing oracle_id {}", r.oracle_id))?;
                Ok((strip0x(&r.oracle_key).to_ascii_lowercase(), id))
            })
            .collect()
    }

    /// Fetch a signed consensus payload for `feed_hashes`.
    ///
    /// `oracle_objects` comes from [`CrossbarClient::sui_oracles`].
    pub async fn fetch_quotes(
        &self,
        feed_hashes: &[String],
        oracle_objects: &BTreeMap<String, ObjectID>,
    ) -> Result<QuoteBundle> {
        validate_request(feed_hashes)?;
        let joined = feed_hashes
            .iter()
            .map(|h| strip0x(h).to_string())
            .collect::<Vec<_>>()
            .join(",");
        let url = format!("{}/v2/update/{joined}{}", self.base_url, self.network_query());
        debug!(%url, feeds = feed_hashes.len(), "fetching switchboard consensus payload");
        let resp: UpdateResponse = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("GET {url}"))?
            .json()
            .await
            .context("decoding crossbar /v2/update payload")?;
        resp.into_bundle(oracle_objects)
    }

    /// Enforce "every asset we intend to price actually has a feed"
    /// before spending a round trip.
    pub fn require_feeds<'a>(
        feed_hashes: &'a BTreeMap<String, String>,
        assets: &[String],
    ) -> Result<Vec<&'a str>> {
        assets
            .iter()
            .map(|a| {
                feed_hashes
                    .get(a)
                    .map(|s| s.as_str())
                    .ok_or_else(|| anyhow!("no switchboard feed hash for {a}"))
            })
            .collect()
    }
}

// ── on-chain oracle map (SO-346) ─────────────────────────────────────

/// Resolve `oracle_key (lowercase hex) → Sui Oracle OBJECT id` from the
/// chain itself: the Switchboard `Queue`'s `existing_oracles` table.
///
/// Crossbar's own `GET /oracles/sui` cannot serve Sui TESTNET: its
/// per-chain refresh routines are hardwired to the public Sui fullnodes
/// (no env override, and the testnet one has been unusable since
/// 2026-07), and `?network=devnet` lists Sui DEVNET objects — ids that
/// do not exist on testnet. The chain is authoritative anyway: `run_N`
/// validates signing oracles against this exact table.
pub async fn oracles_from_queue(
    sui_rpc_url: &str,
    queue_id: ObjectID,
) -> Result<BTreeMap<String, ObjectID>> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("building sui rpc http client");
    let rpc = |method: &'static str, params: serde_json::Value| {
        let http = http.clone();
        let url = sui_rpc_url.to_string();
        async move {
            let resp: serde_json::Value = http
                .post(&url)
                .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
                .send()
                .await
                .with_context(|| format!("POST {url} {method}"))?
                .error_for_status()
                .with_context(|| format!("POST {url} {method}"))?
                .json()
                .await
                .with_context(|| format!("decoding {method} response"))?;
            if let Some(err) = resp.get("error") {
                return Err(anyhow!("{method} RPC error: {err}"));
            }
            resp.get("result")
                .cloned()
                .ok_or_else(|| anyhow!("{method} response has no result"))
        }
    };

    // Queue object → its existing_oracles table id.
    let queue = rpc(
        "sui_getObject",
        serde_json::json!([queue_id.to_hex_literal(), {"showContent": true}]),
    )
    .await?;
    let table_id = queue
        .pointer("/data/content/fields/existing_oracles/fields/id/id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("queue {queue_id} has no existing_oracles table"))?
        .to_string();

    // Paginate the table's dynamic fields.
    let mut field_ids: Vec<String> = Vec::new();
    let mut cursor = serde_json::Value::Null;
    loop {
        let page = rpc(
            "suix_getDynamicFields",
            serde_json::json!([table_id, cursor, 50]),
        )
        .await?;
        for row in page.pointer("/data").and_then(|v| v.as_array()).into_iter().flatten() {
            if let Some(id) = row.get("objectId").and_then(|v| v.as_str()) {
                field_ids.push(id.to_string());
            }
        }
        if !page.pointer("/hasNextPage").and_then(|v| v.as_bool()).unwrap_or(false) {
            break;
        }
        cursor = page.pointer("/nextCursor").cloned().unwrap_or(serde_json::Value::Null);
    }
    if field_ids.is_empty() {
        return Err(anyhow!("queue {queue_id} existing_oracles table is empty"));
    }

    // Batch-read the entries.
    let mut out = BTreeMap::new();
    for chunk in field_ids.chunks(50) {
        let objs = rpc(
            "sui_multiGetObjects",
            serde_json::json!([chunk, {"showContent": true}]),
        )
        .await?;
        for obj in objs.as_array().into_iter().flatten() {
            let Some(fields) = obj.pointer("/data/content/fields") else {
                continue;
            };
            let (key, id) = parse_existing_oracle(fields)?;
            out.insert(key, id);
        }
    }
    Ok(out)
}

/// One `existing_oracles` dynamic-field entry → (oracle_key hex, object id).
fn parse_existing_oracle(fields: &serde_json::Value) -> Result<(String, ObjectID)> {
    let key_bytes: Vec<u8> = fields
        .pointer("/name")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("existing_oracles entry has no byte-vector name"))?
        .iter()
        .map(|b| {
            b.as_u64()
                .and_then(|n| u8::try_from(n).ok())
                .ok_or_else(|| anyhow!("oracle key byte out of range"))
        })
        .collect::<Result<_>>()?;
    let oracle_id = fields
        .pointer("/value/fields/oracle_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("existing_oracles entry has no oracle_id"))?;
    Ok((
        hex::encode(key_bytes),
        ObjectID::from_hex_literal(oracle_id)
            .with_context(|| format!("parsing oracle_id {oracle_id:?}"))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_oracle_entry_parses() {
        // Shape captured live from the testnet queue's existing_oracles
        // table (sui_getObject on a dynamic-field entry, 2026-08-04).
        let fields: serde_json::Value = serde_json::json!({
            "name": [0x3f, 0x90, 0xb0, 0xe2],
            "value": {"fields": {
                "oracle_id": "0x6f196ceeabed0a60cb1f9675b0f1ef055092e8af674c9ce7a4516057b4aa5338",
                "oracle_key": [0x3f, 0x90, 0xb0, 0xe2]
            }}
        });
        let (key, id) = parse_existing_oracle(&fields).unwrap();
        assert_eq!(key, "3f90b0e2");
        assert_eq!(
            id,
            ObjectID::from_hex_literal(
                "0x6f196ceeabed0a60cb1f9675b0f1ef055092e8af674c9ce7a4516057b4aa5338"
            )
            .unwrap()
        );
    }

    #[test]
    fn network_query_shapes_the_update_url() {
        let none = CrossbarClient::new("http://x", None);
        assert_eq!(none.network_query(), "");
        let dev = CrossbarClient::new("http://x/", Some("devnet".into()));
        assert_eq!(dev.network_query(), "?network=devnet");
    }

    /// Trimmed from a real `GET /v2/update/{btc}` response against
    /// crossbar.switchboard.xyz — the shape this decoder must survive.
    const LIVE_SAMPLE: &str = r#"{
      "medianResponses": [
        {"value":"63456010000000000000000","feedHash":"4cd1cad962425681af07b9254b7d804de3ca3446fbfd1371bb258d2c75059812","numOracles":1}
      ],
      "oracleResponses": [
        {"oraclePubkey":"405a6ee0581e9bb6037232cfc7318590752f05f769821aa7c18bcd2edf291e89",
         "ethAddress":"2d385803c1af442704d50ecb6e600700d86cc747",
         "signature":"NMbdWaCa6wkqdp+qyYb87/S8KriXz74rfcKanmqZOA8xr3H3yzFqsLav+HIPCx+xK+TaR6Ng2aKpxJmtQEg5cg==",
         "recoveryId":1,
         "feedResponses":[{"feed_hash":"4cd1cad962425681af07b9254b7d804de3ca3446fbfd1371bb258d2c75059812","min_oracle_samples":1,"queue_pubkey":"86807068432f186a147cf0b13a30067d386204ea9d6c8b04743ac2ef010b0752"}]}
      ],
      "timestamp": 1785700471,
      "slot": 42,
      "recentHash": "7jJdiQF3MgcUJKmTw4dFWD5gp8PFQ3JrL624njAuzyif"
    }"#;

    fn oracle_map() -> BTreeMap<String, ObjectID> {
        let mut m = BTreeMap::new();
        m.insert(
            "405a6ee0581e9bb6037232cfc7318590752f05f769821aa7c18bcd2edf291e89".into(),
            ObjectID::from_hex_literal(
                "0xcbe815280222f191e7b9ebeeb4e19db039967bd70e753bdf8fadc361603ee751",
            )
            .unwrap(),
        );
        m
    }

    #[test]
    fn decodes_a_live_crossbar_payload() {
        let resp: UpdateResponse = serde_json::from_str(LIVE_SAMPLE).unwrap();
        let b = resp.into_bundle(&oracle_map()).unwrap();

        assert_eq!(b.feed_ids.len(), 1);
        assert_eq!(b.feed_ids[0].len(), 32);
        // $63,456.01 at 18 decimals.
        assert_eq!(b.values, vec![63_456_010_000_000_000_000_000u128]);
        assert_eq!(b.values_neg, vec![false]);
        assert_eq!(b.min_oracle_samples, vec![1]);
        assert_eq!(b.slot, 42);
        assert_eq!(b.timestamp_seconds, 1_785_700_471);
        // base64, not hex — decoding it as hex would silently yield junk.
        assert_eq!(b.signatures.len(), 1);
        assert_eq!(b.signatures[0].len(), 64);
        assert_eq!(b.oracle_ids.len(), 1);
    }

    #[test]
    fn an_unmapped_signing_oracle_is_an_error_not_a_silent_drop() {
        // Dropping the signer would quietly shrink the consensus set the
        // on-chain verifier checks against.
        let resp: UpdateResponse = serde_json::from_str(LIVE_SAMPLE).unwrap();
        let err = resp.into_bundle(&BTreeMap::new()).unwrap_err().to_string();
        assert!(err.contains("no Sui object"), "{err}");
    }

    #[test]
    fn eighteen_decimal_values_survive_json() {
        // 6.3e22 is far past u64 and past f64's exact range, so the
        // string path is the only correct one.
        let resp: UpdateResponse = serde_json::from_str(LIVE_SAMPLE).unwrap();
        let b = resp.into_bundle(&oracle_map()).unwrap();
        assert!(b.values[0] > u64::MAX as u128);
    }

    #[test]
    fn feed_hashes_must_be_32_bytes_of_hex() {
        assert!(validate_request(&[]).is_err());
        assert!(validate_request(&["nothex".into()]).is_err());
        assert!(validate_request(&["0xdeadbeef".into()]).is_err());
        assert!(validate_request(&[format!("0x{}", "ab".repeat(32))]).is_ok());
        // The `0x` prefix is optional on the way in.
        assert!(validate_request(&["ab".repeat(32)]).is_ok());
    }

    #[test]
    fn too_many_signing_oracles_is_refused() {
        let mut resp: UpdateResponse = serde_json::from_str(LIVE_SAMPLE).unwrap();
        let sig = resp.oracle_responses.remove(0).signature;
        let mut map = oracle_map();
        for i in 0..=MAX_ORACLES {
            let key = format!("{i:064x}");
            resp.oracle_responses.push(OracleResponse {
                oracle_pubkey: key.clone(),
                signature: sig.clone(),
                feed_responses: vec![FeedResponse {
                    feed_hash: "4cd1cad962425681af07b9254b7d804de3ca3446fbfd1371bb258d2c75059812"
                        .into(),
                    min_oracle_samples: 1,
                    queue_pubkey: Some(
                        "86807068432f186a147cf0b13a30067d386204ea9d6c8b04743ac2ef010b0752".into(),
                    ),
                }],
            });
            map.insert(key, ObjectID::ZERO);
        }
        let err = resp.into_bundle(&map).unwrap_err().to_string();
        assert!(err.contains("run_1..run_6"), "{err}");
    }

    /// The queue mismatch this guard exists for is REAL: the public
    /// crossbar answers for queue 8680… while Sui testnet's on-chain
    /// oracle queue is c9477bfb…. Submitting across them aborts inside
    /// run_N with nothing useful in the error.
    #[test]
    fn a_cross_queue_bundle_is_refused_with_both_queues_named() {
        let resp: UpdateResponse = serde_json::from_str(LIVE_SAMPLE).unwrap();
        let b = resp.into_bundle(&oracle_map()).unwrap();
        assert_eq!(
            b.queue_key,
            "86807068432f186a147cf0b13a30067d386204ea9d6c8b04743ac2ef010b0752"
        );

        // Sui testnet's real oracle queue key.
        let err = b
            .require_queue("0xc9477bfb5ff1012859f336cf98725680e7705ba2abece17188cfb28ca66ca5b0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("86807068"), "{err}");
        assert!(err.contains("c9477bfb"), "{err}");

        // Matching queue passes, with or without the 0x and in any case.
        b.require_queue("0x86807068432F186A147CF0B13A30067D386204EA9D6C8B04743AC2EF010B0752")
            .unwrap();
    }

    #[test]
    fn a_bundle_with_no_queue_is_refused() {
        // An unverifiable queue is precisely the case require_queue
        // exists for, so defaulting it would defeat the guard.
        let mut resp: UpdateResponse = serde_json::from_str(LIVE_SAMPLE).unwrap();
        for o in &mut resp.oracle_responses {
            for fr in &mut o.feed_responses {
                fr.queue_pubkey = None;
            }
        }
        let err = resp.into_bundle(&oracle_map()).unwrap_err().to_string();
        assert!(err.contains("no queue_pubkey"), "{err}");
    }

    #[test]
    fn require_feeds_names_the_missing_asset() {
        let mut m = BTreeMap::new();
        m.insert("0x1::a::A".to_string(), "0xaa".to_string());
        let err = CrossbarClient::require_feeds(&m, &["0x1::b::B".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("0x1::b::B"), "{err}");
        assert!(CrossbarClient::require_feeds(&m, &["0x1::a::A".to_string()]).is_ok());
    }
}
