//! Client for **Crossbar**, Switchboard's utility server (SO-335).
//!
//! Crossbar is what Hermes is to Pyth: the off-chain endpoint that turns
//! a feed identifier into signed oracle data we can submit on chain. We
//! run our own (`switchboardlabs/rust-crossbar`, added in SO-333 and
//! reachable at `/{env}/crossbar/`) rather than the public instance,
//! which is rate-limited.
//!
//! The one call that matters is [`CrossbarClient::fetch_quotes`]: given
//! feed hashes it returns a [`QuoteBundle`] — the signed oracle
//! responses, in exactly the shape
//! `switchboard::quote_submit_result_action::run_N` consumes. The
//! bundle is then handed to `sui_tx::tx::oracle::switchboard`, which
//! lays it into the PTB.
//!
//! ## Note on the response shape
//!
//! Crossbar's on-chain-update responses are documented per chain and
//! Sui's is not pinned in the public docs. [`QuoteBundle`] therefore
//! decodes defensively: hex-or-array byte fields, and string-or-number
//! integers, both of which appear across Crossbar's chain-specific
//! encodings. Anything that does not fit is a hard error rather than a
//! silent default — a mis-decoded quote is a mispriced book.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use sui_types::base_types::ObjectID;
use tracing::debug;

/// Signed oracle data for one or more feeds, ready for on-chain submit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteBundle {
    pub feed_ids: Vec<Vec<u8>>,
    pub values: Vec<u128>,
    pub values_neg: Vec<bool>,
    pub min_oracle_samples: Vec<u8>,
    pub signatures: Vec<Vec<u8>>,
    pub slot: u64,
    pub timestamp_seconds: u64,
    pub oracle_ids: Vec<ObjectID>,
    pub queue_id: ObjectID,
}

/// Byte field that may arrive as `0x`-hex or as a JSON array of octets.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Bytes {
    Hex(String),
    Array(Vec<u8>),
}

impl Bytes {
    fn into_vec(self) -> Result<Vec<u8>> {
        match self {
            Bytes::Hex(s) => {
                hex::decode(s.trim().trim_start_matches("0x")).context("decoding hex byte field")
            }
            Bytes::Array(v) => Ok(v),
        }
    }
}

/// Integer that may arrive as a JSON number or a decimal string.
///
/// The numeric variant is `u64`, not `u128`, for two reasons: serde's
/// untagged enums cannot deserialize `u128`, and a JSON *number* that
/// large is already lossy by the time it reaches us. Switchboard's
/// 18-decimal values exceed `u64` (a $1,500 price is 1.5e21), so in
/// practice they always arrive as strings — which this handles exactly.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Num {
    U64(u64),
    Str(String),
}

impl Num {
    fn as_u128(&self) -> Result<u128> {
        match self {
            Num::U64(v) => Ok(*v as u128),
            Num::Str(s) => s.trim().parse::<u128>().context("parsing numeric string"),
        }
    }
    fn as_u64(&self) -> Result<u64> {
        u64::try_from(self.as_u128()?).context("numeric field does not fit u64")
    }
}

#[derive(Debug, Deserialize)]
struct RawQuote {
    feed_id: Bytes,
    value: Num,
    #[serde(default)]
    neg: bool,
    #[serde(default = "one")]
    min_oracle_samples: u8,
}

fn one() -> u8 {
    1
}

#[derive(Debug, Deserialize)]
struct RawBundle {
    quotes: Vec<RawQuote>,
    signatures: Vec<Bytes>,
    oracle_ids: Vec<String>,
    queue_id: String,
    slot: Num,
    timestamp_seconds: Num,
}

impl RawBundle {
    fn into_bundle(self) -> Result<QuoteBundle> {
        let mut feed_ids = Vec::with_capacity(self.quotes.len());
        let mut values = Vec::with_capacity(self.quotes.len());
        let mut values_neg = Vec::with_capacity(self.quotes.len());
        let mut min_oracle_samples = Vec::with_capacity(self.quotes.len());
        for q in self.quotes {
            feed_ids.push(q.feed_id.into_vec()?);
            values.push(q.value.as_u128()?);
            values_neg.push(q.neg);
            min_oracle_samples.push(q.min_oracle_samples);
        }
        let signatures = self
            .signatures
            .into_iter()
            .map(|s| s.into_vec())
            .collect::<Result<Vec<_>>>()?;
        let oracle_ids = self
            .oracle_ids
            .iter()
            .map(|s| ObjectID::from_hex_literal(s.trim()).context("parsing oracle id"))
            .collect::<Result<Vec<_>>>()?;
        Ok(QuoteBundle {
            feed_ids,
            values,
            values_neg,
            min_oracle_samples,
            signatures,
            slot: self.slot.as_u64()?,
            timestamp_seconds: self.timestamp_seconds.as_u64()?,
            oracle_ids,
            queue_id: ObjectID::from_hex_literal(self.queue_id.trim())
                .context("parsing queue id")?,
        })
    }
}

/// Guard clauses for [`CrossbarClient::fetch_quotes`], split out so they
/// are testable without a live Crossbar.
fn validate_request(feed_hashes: &[String], num_oracles: usize) -> Result<()> {
    if feed_hashes.is_empty() {
        return Err(anyhow!("fetch_quotes called with no feed hashes"));
    }
    if num_oracles == 0 || num_oracles > MAX_ORACLES {
        return Err(anyhow!(
            "num_oracles must be 1..={MAX_ORACLES} (the on-chain run_N arities), got {num_oracles}"
        ));
    }
    Ok(())
}

/// Largest `run_N` arity `switchboard::quote_submit_result_action`
/// exposes. Asking Crossbar for more signatures than that would build a
/// PTB calling a function the package does not export.
pub const MAX_ORACLES: usize = 6;

#[derive(Debug, Clone)]
pub struct CrossbarClient {
    base_url: String,
    http: reqwest::Client,
}

impl CrossbarClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("building crossbar http client"),
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

    /// Fetch signed quotes for `feed_hashes`.
    ///
    /// `num_oracles` bounds how many signatures come back, which in turn
    /// selects the `run_N` arity on chain — the Move package exposes
    /// `run_1..run_6`, so values outside that are rejected here rather
    /// than producing a PTB that calls a function which does not exist.
    pub async fn fetch_quotes(
        &self,
        feed_hashes: &[String],
        num_oracles: usize,
    ) -> Result<QuoteBundle> {
        validate_request(feed_hashes, num_oracles)?;
        let url = format!("{}/updates/sui/quotes", self.base_url);
        let body = serde_json::json!({
            "feedHashes": feed_hashes,
            "numOracles": num_oracles,
        });
        debug!(%url, feeds = feed_hashes.len(), num_oracles, "fetching switchboard quotes");
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?
            .error_for_status()
            .with_context(|| format!("POST {url}"))?;
        let raw: RawBundle = resp.json().await.context("decoding crossbar quote bundle")?;
        raw.into_bundle()
    }

    /// Resolve feed hashes for a set of coin types from the catalog map
    /// the caller already holds. Present so callers have one place to
    /// enforce "every asset we intend to price actually has a feed"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_and_array_byte_fields_both_decode() {
        let hex: Bytes = serde_json::from_str("\"0x0a0b\"").unwrap();
        assert_eq!(hex.into_vec().unwrap(), vec![10, 11]);
        let arr: Bytes = serde_json::from_str("[10, 11]").unwrap();
        assert_eq!(arr.into_vec().unwrap(), vec![10, 11]);
    }

    #[test]
    fn stringified_and_numeric_integers_both_decode() {
        // u128 values are routinely stringified to survive JSON's 2^53.
        let s: Num = serde_json::from_str("\"340282366920938463463374607431768211455\"").unwrap();
        assert_eq!(s.as_u128().unwrap(), u128::MAX);
        let n: Num = serde_json::from_str("42").unwrap();
        assert_eq!(n.as_u64().unwrap(), 42);
    }

    #[test]
    fn oversized_integer_is_rejected_not_truncated() {
        let n: Num = serde_json::from_str("\"18446744073709551616\"").unwrap();
        assert!(n.as_u64().is_err(), "u64 overflow must error, not wrap");
    }

    #[test]
    fn a_full_bundle_round_trips() {
        let json = serde_json::json!({
            "quotes": [
                {"feed_id": "0xaa", "value": "1500000000000000000000", "neg": false, "min_oracle_samples": 3},
                {"feed_id": [1, 2], "value": 7}
            ],
            "signatures": ["0xdeadbeef", "0xfeed"],
            "oracle_ids": ["0x1", "0x2"],
            "queue_id": "0x3",
            "slot": 99,
            "timestamp_seconds": "1750000000"
        });
        let raw: RawBundle = serde_json::from_value(json).unwrap();
        let b = raw.into_bundle().unwrap();
        assert_eq!(b.feed_ids, vec![vec![0xaa], vec![1, 2]]);
        assert_eq!(b.values, vec![1_500_000_000_000_000_000_000u128, 7]);
        assert_eq!(b.values_neg, vec![false, false]);
        // Defaulted, because a missing sample count means "at least one".
        assert_eq!(b.min_oracle_samples, vec![3, 1]);
        assert_eq!(b.signatures.len(), 2);
        assert_eq!(b.oracle_ids.len(), 2);
        assert_eq!(b.slot, 99);
        assert_eq!(b.timestamp_seconds, 1_750_000_000);
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

    #[test]
    fn out_of_range_oracle_counts_are_refused_before_the_round_trip() {
        // run_N only exists for N in 1..=6, so a bad count must fail here
        // rather than on chain.
        assert!(validate_request(&["0xaa".into()], 0).is_err());
        assert!(validate_request(&["0xaa".into()], MAX_ORACLES + 1).is_err());
        assert!(validate_request(&[], 3).is_err());
        assert!(validate_request(&["0xaa".into()], 1).is_ok());
        assert!(validate_request(&["0xaa".into()], MAX_ORACLES).is_ok());
    }
}
