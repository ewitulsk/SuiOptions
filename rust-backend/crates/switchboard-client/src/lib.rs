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

/// Largest `run_N` arity `switchboard::quote_submit_action`
/// exposes. More signatures than that cannot be submitted on chain.
pub const MAX_ORACLES: usize = 6;

/// Signed oracle data for one or more feeds, ready for on-chain submit.
///
/// Field-for-field what `quote_submit_action::run_N` consumes;
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
    /// base64, 64 bytes (r || s). The recovery byte rides SEPARATELY.
    signature: String,
    /// On-chain verification is `ecdsa_k1::secp256k1_ecrecover`, whose
    /// signature argument is 65 bytes (r || s || v) — dropping this byte
    /// makes every quote abort inside the native with code 1 (observed
    /// live before it was appended).
    ///
    /// CAUTION: the reported value is unreliable. Observed live: every
    /// bundle crossbar reported with recoveryId 1 failed on-chain
    /// recovery (the wrong v recovers a DIFFERENT key, the oracle is
    /// silently skipped, and `Quotes` comes out empty — surfacing as
    /// E_FEED_MISSING_FROM_BUNDLE downstream). The id is therefore
    /// RE-DERIVED against `ethAddress` before submit; this field is only
    /// the first candidate tried.
    #[serde(rename = "recoveryId", default)]
    recovery_id: u8,
    /// keccak160 of the signer's uncompressed secp key — lets the client
    /// verify which recovery id is actually correct.
    #[serde(rename = "ethAddress", default)]
    eth_address: Option<String>,
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

/// One simulated feed value from `GET /v2/simulate` (SO-353).
#[derive(Debug, Clone, PartialEq)]
pub struct SimulatedPrice {
    /// Lowercase hex, no `0x`.
    pub feed_hash: String,
    /// Median of the feed's job results, in natural units (USD etc.).
    pub value: f64,
}

#[derive(Debug, Deserialize)]
struct SimulateResponse {
    #[serde(default)]
    feeds: Vec<SimulatedFeed>,
}

#[derive(Debug, Deserialize)]
struct SimulatedFeed {
    #[serde(rename = "feedHash")]
    feed_hash: String,
    /// `Option` because a failed feed carries an explicit `null` (which
    /// `#[serde(default)]` alone would reject).
    #[serde(default)]
    results: Option<Vec<SimValue>>,
}

impl SimulatedFeed {
    fn median_value(&self) -> Option<f64> {
        median(
            self.results
                .as_deref()
                .unwrap_or_default()
                .iter()
                .filter_map(SimValue::as_f64),
        )
    }
}

/// Simulate results arrive as decimal strings today, but crossbar's Sui
/// shapes are unpinned (see `fetch_quotes`) — accept numbers too.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SimValue {
    Str(String),
    Num(f64),
}

impl SimValue {
    fn as_f64(&self) -> Option<f64> {
        let v = match self {
            SimValue::Str(s) => s.trim().parse::<f64>().ok()?,
            SimValue::Num(n) => *n,
        };
        v.is_finite().then_some(v)
    }
}

/// Median of an f64 stream; `None` when empty. Even count averages the
/// two middles.
fn median(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut v: Vec<f64> = values.collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).expect("finite values"));
    let mid = v.len() / 2;
    Some(if v.len() % 2 == 1 {
        v[mid]
    } else {
        (v[mid - 1] + v[mid]) / 2.0
    })
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
    fn into_bundle(self, oracle_objects: &BTreeMap<String, OracleInfo>) -> Result<QuoteBundle> {
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
        let min_oracle_samples: Vec<u8> = self
            .median_responses
            .iter()
            .map(|m| {
                *min_samples
                    .get(&strip0x(&m.feed_hash).to_ascii_lowercase())
                    .unwrap_or(&1)
            })
            .collect();

        // The exact preimage `run_N` verifies: slot ‖ timestamp ‖ per-feed
        // (feed_id(32) ‖ value as i128 two's-complement LE ‖ min_samples).
        // Layout confirmed against the PUBLISHED module's disassembly.
        // The ecrecover native is called with hash id 1 = SHA256 (0 is
        // keccak in sui::ecdsa_k1 — easy to get backwards).
        let prehash = {
            use sha2::Digest;
            let mut m = Vec::new();
            m.extend_from_slice(&self.slot.to_le_bytes());
            m.extend_from_slice(&self.timestamp.to_le_bytes());
            for i in 0..feed_ids.len() {
                m.extend_from_slice(&feed_ids[i]);
                let signed: i128 =
                    if values_neg[i] { -(values[i] as i128) } else { values[i] as i128 };
                m.extend_from_slice(&signed.to_le_bytes());
                m.push(min_oracle_samples[i]);
            }
            let out = sha2::Sha256::digest(&m);
            let mut h = [0u8; 32];
            h.copy_from_slice(&out);
            h
        };

        let mut signatures = Vec::with_capacity(self.oracle_responses.len());
        let mut oracle_ids = Vec::with_capacity(self.oracle_responses.len());
        for o in &self.oracle_responses {
            let mut sig = base64::engine::general_purpose::STANDARD
                .decode(o.signature.trim())
                .context("decoding base64 oracle signature")?;
            if sig.len() != 64 {
                return Err(anyhow!(
                    "oracle signature is {} bytes; expected 64 (r||s) plus a separate recoveryId",
                    sig.len()
                ));
            }
            // r || s || v — the shape `secp256k1_ecrecover` takes on chain.
            // v is re-derived against ethAddress when possible; the
            // reported id is only the first candidate (see OracleResponse).
            let key = strip0x(&o.oracle_pubkey).to_ascii_lowercase();
            let info = oracle_objects.get(&key).ok_or_else(|| {
                anyhow!(
                    "oracle {key} signed the bundle but is not in the queue's on-chain \
                     registered-oracle map — cannot build the on-chain call"
                )
            })?;
            let v = resolve_recovery_id(&prehash, &sig, o.recovery_id, &info.secp_key)
                .with_context(|| format!("oracle {key} (object {})", info.object_id))?;
            sig.push(v);
            signatures.push(sig);
            oracle_ids.push(info.object_id);
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

/// Pick the recovery id whose recovered signer matches the oracle's
/// ON-CHAIN attested secp key — the exact comparison `run_N` makes
/// (SO-346).
///
/// Two live failure modes this kills:
/// - crossbar's reported `recoveryId` is sometimes wrong; the wrong v
///   recovers a different key and the oracle is silently dropped;
/// - crossbar rotates across signers whose Sui objects hold ZERO or
///   stale `secp256k1_key`s (most of the testnet queue, observed
///   2026-08-04) — those bundles can never verify on chain, so refusing
///   them client-side lets the caller retry for an attested signer.
fn resolve_recovery_id(
    prehash: &[u8; 32],
    rs: &[u8],
    reported: u8,
    onchain_secp: &[u8],
) -> Result<u8> {
    if onchain_secp.is_empty() || onchain_secp.iter().all(|b| *b == 0) {
        return Err(anyhow!(
            "signing oracle's on-chain secp256k1_key is absent/zero — its attestation \
             never landed on this network; retry for a different signer"
        ));
    }
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
    // Ethereum-style 27/28 spellings normalize to parity.
    let base = if reported >= 27 { reported - 27 } else { reported } & 1;
    let sig = Signature::from_slice(rs).context("parsing r||s signature")?;
    for cand in [base, 1 - base] {
        let Ok(rid) = RecoveryId::try_from(cand) else { continue };
        let Ok(vk) = VerifyingKey::recover_from_prehash(prehash, &sig, rid) else {
            continue;
        };
        // On chain: check_subvec(decompressed_pubkey, secp_key, 1) — the
        // attested key is compared against the uncompressed key from
        // offset 1 (past the 0x04 prefix).
        let uncompressed = vk.to_encoded_point(false);
        let body = &uncompressed.as_bytes()[1..];
        if body.len() >= onchain_secp.len() && &body[..onchain_secp.len()] == onchain_secp {
            if cand != base {
                tracing::warn!(
                    reported,
                    used = cand,
                    "crossbar recoveryId was wrong; corrected against the on-chain key"
                );
            }
            return Ok(cand);
        }
    }
    Err(anyhow!(
        "signature does not recover the oracle's on-chain secp key under either \
         recovery id — stale attestation or wrong signer; retry for a different signer"
    ))
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
    /// `oracle_objects` comes from [`oracles_from_queue`] — the chain map
    /// carrying each oracle's attested secp key.
    pub async fn fetch_quotes(
        &self,
        feed_hashes: &[String],
        oracle_objects: &BTreeMap<String, OracleInfo>,
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

    /// `GET /v2/simulate/{hashes}` — UNSIGNED price reads for the data
    /// plane (SO-353).
    ///
    /// Unlike `/v2/update` this needs no signing oracles (it reads
    /// crossbar's own live Surge exchange stream), so it keeps serving
    /// when the oracle cache is empty. Response shape captured live
    /// 2026-08-06:
    ///
    /// ```json
    /// {"feeds":[{"feedHash":"4cd1…","feedName":"Surge Stream BTC/USD, WEIGHTED",
    ///            "results":["64488.9"],"receipts":null,"network":"mainnet"}],
    ///  "totalFeeds":1,"successfulFeeds":1,"failedFeeds":0}
    /// ```
    ///
    /// `results` holds one value per job run; string-or-number is decoded
    /// defensively like the rest of crossbar's unpinned shapes. A feed
    /// with no parseable results is skipped, not an error — its staleness
    /// is the consumer's signal, and one bad feed must not blank the rest.
    pub async fn simulate(&self, feed_hashes: &[String]) -> Result<Vec<SimulatedPrice>> {
        validate_request(feed_hashes)?;
        let joined = feed_hashes
            .iter()
            .map(|h| strip0x(h).to_string())
            .collect::<Vec<_>>()
            .join(",");
        let url = format!("{}/v2/simulate/{joined}", self.base_url);
        let resp: SimulateResponse = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("GET {url}"))?
            .json()
            .await
            .context("decoding crossbar /v2/simulate payload")?;
        Ok(resp
            .feeds
            .into_iter()
            .filter_map(|f| {
                let value = f.median_value()?;
                Some(SimulatedPrice {
                    feed_hash: strip0x(&f.feed_hash).to_ascii_lowercase(),
                    value,
                })
            })
            .collect())
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
/// One registered oracle: its Sui object and the secp256k1 signing key
/// the object currently attests. `run_N` recovers each signature and
/// compares against THIS key — a signer whose object holds a zero/stale
/// key is silently dropped on chain, so the client must check it too.
/// Observed live (2026-08-04): most of the testnet queue's 37 oracles
/// carry all-zero keys; only properly-attested signers verify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleInfo {
    pub object_id: ObjectID,
    /// The object's `secp256k1_key` bytes (X‖Y or X-prefix form; compared
    /// against the recovered key's uncompressed bytes from offset 1).
    pub secp_key: Vec<u8>,
}

pub async fn oracles_from_queue(
    sui_rpc_url: &str,
    queue_id: ObjectID,
) -> Result<BTreeMap<String, OracleInfo>> {
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
    let mut by_key: BTreeMap<String, ObjectID> = BTreeMap::new();
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
            by_key.insert(key, id);
        }
    }

    // Batch-read the oracle OBJECTS for their attested secp keys.
    let mut out = BTreeMap::new();
    let entries: Vec<(String, ObjectID)> = by_key.into_iter().collect();
    for chunk in entries.chunks(50) {
        let ids: Vec<String> = chunk.iter().map(|(_, id)| id.to_hex_literal()).collect();
        let objs = rpc(
            "sui_multiGetObjects",
            serde_json::json!([ids, {"showContent": true}]),
        )
        .await?;
        let arr = objs.as_array().cloned().unwrap_or_default();
        for ((key, id), obj) in chunk.iter().zip(arr) {
            let secp_key: Vec<u8> = obj
                .pointer("/data/content/fields/secp256k1_key")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|b| b.as_u64().map(|n| n as u8)).collect())
                .unwrap_or_default();
            out.insert(key.clone(), OracleInfo { object_id: *id, secp_key });
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

    /// Captured live from our staging crossbar (`GET /v2/simulate/{4 hashes}`,
    /// 2026-08-06) — one feed trimmed to keep the fixture small.
    #[test]
    fn simulate_response_parses_live_shape() {
        let body = r#"{
            "feeds": [
                {"feedHash":"4cd1cad962425681af07b9254b7d804de3ca3446fbfd1371bb258d2c75059812",
                 "feedName":"Surge Stream BTC/USD, WEIGHTED",
                 "results":["64477.6"],"receipts":null,"network":"mainnet"},
                {"feedHash":"0x580de69fa5310460bead69dc3fd0c05988dea014d0e7c98aae22b67e7958fd9b",
                 "feedName":"Surge Stream WAL/USD, WEIGHTED",
                 "results":["0.02538"],"receipts":null,"network":"mainnet"}
            ],
            "totalFeeds": 2, "successfulFeeds": 2, "failedFeeds": 0
        }"#;
        let resp: SimulateResponse = serde_json::from_str(body).unwrap();
        let prices: Vec<SimulatedPrice> = resp
            .feeds
            .into_iter()
            .filter_map(|f| {
                let value = f.median_value()?;
                Some(SimulatedPrice {
                    feed_hash: strip0x(&f.feed_hash).to_ascii_lowercase(),
                    value,
                })
            })
            .collect();
        assert_eq!(prices.len(), 2);
        assert!((prices[0].value - 64_477.6).abs() < 1e-9);
        // 0x prefix normalized away so cache keys compare byte-equal.
        assert_eq!(
            prices[1].feed_hash,
            "580de69fa5310460bead69dc3fd0c05988dea014d0e7c98aae22b67e7958fd9b"
        );
    }

    /// A failed feed (null results) is skipped, not an error; numeric
    /// results are accepted alongside strings.
    #[test]
    fn simulate_skips_failed_feeds_and_accepts_numbers() {
        let body = r#"{
            "feeds": [
                {"feedHash":"aa","results":null},
                {"feedHash":"bb","results":["not-a-number"]},
                {"feedHash":"cc","results":[1.5, "2.5", 3.5]}
            ]
        }"#;
        let resp: SimulateResponse = serde_json::from_str(body).unwrap();
        let prices: Vec<(String, f64)> = resp
            .feeds
            .into_iter()
            .filter_map(|f| f.median_value().map(|v| (f.feed_hash.clone(), v)))
            .collect();
        assert_eq!(prices, vec![("cc".to_string(), 2.5)]);
    }

    #[test]
    fn median_averages_even_counts() {
        assert_eq!(median([].into_iter()), None);
        assert_eq!(median([3.0, 1.0].into_iter()), Some(2.0));
        assert_eq!(median([3.0, 1.0, 2.0].into_iter()), Some(2.0));
    }

    /// Trimmed from a real `GET /v2/update/{btc}` response against
    /// crossbar.switchboard.xyz — the shape this decoder must survive.
    const LIVE_SAMPLE: &str = r#"{
      "medianResponses": [
            {
                  "value": "63859140000000000000000",
                  "feedHash": "4cd1cad962425681af07b9254b7d804de3ca3446fbfd1371bb258d2c75059812",
                  "numOracles": 1
            }
      ],
      "oracleResponses": [
            {
                  "oraclePubkey": "58fce533fc20e246d3a7d5df9388ac69314af93d580047fb9137cd80ba58e641",
                  "ethAddress": "3a1d30312334330d0cb3468d1b712cfbcd68a7d6",
                  "signature": "d87u02ydLQNQKl3qkFRrychs8MLpcMiqw+M6hhH9bY5ttqbNpiV8MX0CgAPsg9B+qMteEF/GLCg4D4KrVZHBZA==",
                  "recoveryId": 0,
                  "feedResponses": [
                        {
                              "failure_error": "",
                              "feed_hash": "4cd1cad962425681af07b9254b7d804de3ca3446fbfd1371bb258d2c75059812",
                              "min_oracle_samples": 1,
                              "msg": "icjSnHLuVmXE/5srlSlCJS42LkSHkia+oxIfix/Jbl0=",
                              "oracle_pubkey": "58fce533fc20e246d3a7d5df9388ac69314af93d580047fb9137cd80ba58e641",
                              "oracle_signing_pubkey": "ac447e686cc28f6e9186d7760417194a756d5af2d22d65a42d73c968b616d598d7443751f1e7355dd77c9d3da7f405085708b9187aa1a9651d3f869e6907b5cf",
                              "queue_pubkey": "c9477bfb5ff1012859f336cf98725680e7705ba2abece17188cfb28ca66ca5b0",
                              "receipts": [
                                    {
                                          "children": [
                                                {
                                                      "children": [],
                                                      "error": null,
                                                      "task_name": "SwitchboardSurgeTask",
                                                      "task_output": "63859.14000000"
                                                }
                                          ],
                                          "error": null,
                                          "task_name": "",
                                          "task_output": "63859.14000000"
                                    }
                              ],
                              "recent_hash": "HCZ5eK6xJ8Z7MzPEkcjKpYSh7o8N9S489jKWuc2skbCh",
                              "recent_successes_if_failed": [],
                              "recovery_id": 1,
                              "signature": "S6W4oS59zFY5DBK5gkc2h+JMe7HSfB6a+fKdTBEPRfYDyiqtEb8WiV59ncdSLXj0a1icZ88ZmPodqWhfSGW5Qg==",
                              "success_value": "63859140000000000000000",
                              "timestamp": 1785824407
                        }
                  ]
            }
      ],
      "timestamp": 1785824405,
      "slot": 481090766
}"#;

    fn oracle_map() -> BTreeMap<String, OracleInfo> {
        let mut m = BTreeMap::new();
        m.insert(
            "58fce533fc20e246d3a7d5df9388ac69314af93d580047fb9137cd80ba58e641".into(),
            OracleInfo {
                object_id: ObjectID::from_hex_literal(
                    "0xcbe815280222f191e7b9ebeeb4e19db039967bd70e753bdf8fadc361603ee751",
                )
                .unwrap(),
                // The fixture signer's uncompressed key body (X||Y),
                // recovered from the captured signature itself — the
                // shape the on-chain object attests.
                secp_key: hex::decode(
                    "ac447e686cc28f6e9186d7760417194a756d5af2d22d65a42d73c968b616d598\
                     d7443751f1e7355dd77c9d3da7f405085708b9187aa1a9651d3f869e6907b5cf",
                )
                .unwrap(),
            },
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
        assert_eq!(b.values, vec![63_859_140_000_000_000_000_000u128]);
        assert_eq!(b.values_neg, vec![false]);
        assert_eq!(b.min_oracle_samples, vec![1]);
        assert_eq!(b.slot, 481090766);
        assert_eq!(b.timestamp_seconds, 1785824405);
        // base64, not hex — decoding it as hex would silently yield junk.
        // 65 bytes: r || s (64, from `signature`) plus the separate
        // `recoveryId` byte appended — the exact shape
        // `secp256k1_ecrecover` takes on chain.
        assert_eq!(b.signatures.len(), 1);
        assert_eq!(b.signatures[0].len(), 65);
        assert_eq!(b.signatures[0][64], 0);
        assert_eq!(b.oracle_ids.len(), 1);
    }

    #[test]
    fn an_unmapped_signing_oracle_is_an_error_not_a_silent_drop() {
        // Dropping the signer would quietly shrink the consensus set the
        // on-chain verifier checks against.
        let resp: UpdateResponse = serde_json::from_str(LIVE_SAMPLE).unwrap();
        let err = resp.into_bundle(&BTreeMap::new()).unwrap_err().to_string();
        assert!(err.contains("registered-oracle map"), "{err}");
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
                recovery_id: 1,
                eth_address: None,
                feed_responses: vec![FeedResponse {
                    feed_hash: "4cd1cad962425681af07b9254b7d804de3ca3446fbfd1371bb258d2c75059812"
                        .into(),
                    min_oracle_samples: 1,
                    queue_pubkey: Some(
                        "86807068432f186a147cf0b13a30067d386204ea9d6c8b04743ac2ef010b0752".into(),
                    ),
                }],
            });
            // Same real secp key everywhere: the cloned signatures must
            // pass signer verification so the ARITY check is what trips.
            map.insert(
                key,
                OracleInfo {
                    object_id: ObjectID::ZERO,
                    secp_key: hex::decode(
                        "ac447e686cc28f6e9186d7760417194a756d5af2d22d65a42d73c968b616d598\
                         d7443751f1e7355dd77c9d3da7f405085708b9187aa1a9651d3f869e6907b5cf",
                    )
                    .unwrap(),
                },
            );
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
        // The fixture (network=devnet) signs under Sui TESTNET's queue.
        let resp: UpdateResponse = serde_json::from_str(LIVE_SAMPLE).unwrap();
        let b = resp.into_bundle(&oracle_map()).unwrap();
        assert_eq!(
            b.queue_key,
            "c9477bfb5ff1012859f336cf98725680e7705ba2abece17188cfb28ca66ca5b0"
        );

        // The public default (Solana-mainnet) queue must be refused.
        let err = b
            .require_queue("0x86807068432f186a147cf0b13a30067d386204ea9d6c8b04743ac2ef010b0752")
            .unwrap_err()
            .to_string();
        assert!(err.contains("86807068"), "{err}");
        assert!(err.contains("c9477bfb"), "{err}");

        // Matching queue passes, with or without the 0x and in any case.
        b.require_queue("0xC9477BFB5FF1012859F336CF98725680E7705BA2ABECE17188CFB28CA66CA5B0")
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
