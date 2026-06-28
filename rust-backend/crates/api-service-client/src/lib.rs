//! Shared client for the **api-service** (`services/api-service`).
//!
//! api-service is the read model the frontend renders from; it projects the
//! indexer's authoritative bucket state over plain HTTP. The mm-bot uses this
//! client to resolve a bucket's *pricing inputs* (strike, scale, expiry, and
//! the underlying/settlement coin types) from the bucket **address alone**.
//!
//! ## Why the bot looks buckets up instead of trusting the RFQ
//!
//! The quoting service's RFQ broadcast carries only the bucket id — never its
//! strike or coin types. If those rode along on the wire, a malicious or buggy
//! upstream could hand the bot spoofed inputs (e.g. a cheap TWAL strike tagged
//! as the TBTC pair) and trick it into signing a badly-mispriced quote. By
//! resolving the bucket itself against api-service, the bot keeps its own trust
//! boundary: it prices only what the authoritative read model says the bucket
//! is.
//!
//! ## Caching
//!
//! A bucket's pricing inputs are immutable for its lifetime — strike, scale,
//! expiry and coin types never change once it's created (only `total_written` /
//! `exercise_cursor` move, which pricing ignores). So a successful lookup is
//! cached forever; the bulk-view path re-prices the same buckets every refresh
//! and would otherwise hammer api-service.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use parking_lot::RwLock;
use protocol_types::asset::canonicalize_move_type;
use protocol_types::ids::ObjectId;
use serde::Deserialize;
use tracing::debug;

/// The immutable pricing inputs for one bucket, resolved from api-service.
/// Coin types are canonical (`0x`-prefixed, padded) so callers can compare
/// them directly against a canonicalized configured pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BucketPricing {
    pub asset_coin_type: String,
    pub settlement_coin_type: String,
    /// The bucket's fungible option-coin type — the `Call` type argument
    /// for bid/write PTBs. Empty when talking to an api-service that
    /// predates the field. For put buckets this carries the `Put` coin type
    /// (api-service serves the per-bucket option coin under both
    /// `call_coin_type` and `option_coin_type`).
    pub call_coin_type: String,
    /// True when this bucket is a cash-secured put (`option_kind == "put"`).
    /// Defaults to false (call) against an api-service that predates puts.
    pub is_put: bool,
    pub strike: u128,
    pub strike_scale: u8,
    pub expiry_ms: u64,
}

impl BucketPricing {
    /// Cash collateral a put writer must post for `amount` underlying units:
    /// `ceil(amount × strike / 10^strike_scale)`. Meaningless for calls.
    pub fn put_collateral(&self, amount: u64) -> u128 {
        let divisor = 10u128.pow(self.strike_scale as u32);
        let numerator = amount as u128 * self.strike;
        numerator.div_ceil(divisor)
    }
}

/// The subset of `GET /buckets/:bucket_id`'s response we price from. serde
/// ignores the rest of the (display-oriented) payload.
#[derive(Deserialize)]
struct BucketDetailWire {
    asset_coin_type: String,
    settlement_coin_type: String,
    #[serde(default)]
    call_coin_type: String,
    /// Generic per-bucket option coin type (put buckets populate this; for
    /// calls it equals `call_coin_type`). Falls back to `call_coin_type`.
    #[serde(default)]
    option_coin_type: String,
    /// "call" | "put"; absent ⇒ "call".
    #[serde(default)]
    option_kind: String,
    strike_raw: String,
    strike_scale: u8,
    expiry_ms: i64,
}

/// Resolves bucket pricing inputs from api-service, caching immutable results.
pub struct ApiServiceClient {
    http: reqwest::Client,
    base_url: String,
    cache: RwLock<HashMap<ObjectId, BucketPricing>>,
}

impl ApiServiceClient {
    /// `base_url` is the api-service root (no trailing slash needed), e.g.
    /// `http://api-service:9003`.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Resolve one bucket's pricing inputs.
    ///
    /// - `Ok(Some(_))` — the bucket exists and was parsed (served from cache on
    ///   any repeat call).
    /// - `Ok(None)` — api-service returned 404: the bucket is unknown, cleaned,
    ///   or settled-and-gone. The caller declines/omits it.
    /// - `Err(_)` — a transport/parse failure the caller surfaces as a decline.
    pub async fn bucket_pricing(&self, id: ObjectId) -> Result<Option<BucketPricing>> {
        if let Some(hit) = self.cache.read().get(&id).cloned() {
            return Ok(Some(hit));
        }

        let url = format!("{}/buckets/{}", self.base_url, id.to_hex());
        let resp = observability::client::instrumented("api-service", "GET /buckets/:id", |h| {
            self.http.get(&url).headers(h).send()
        })
        .await
        .with_context(|| format!("GET {url}"))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            debug!(bucket = %id.to_hex(), "api-service: bucket not found");
            return Ok(None);
        }
        let resp = resp
            .error_for_status()
            .with_context(|| format!("GET {url} returned an error status"))?;
        let wire: BucketDetailWire = resp
            .json()
            .await
            .with_context(|| format!("decoding bucket detail from {url}"))?;

        // Prefer the generic option_coin_type (put-aware); fall back to the
        // legacy call_coin_type field.
        let option_coin = if !wire.option_coin_type.is_empty() {
            wire.option_coin_type.clone()
        } else {
            wire.call_coin_type.clone()
        };
        let pricing = BucketPricing {
            asset_coin_type: canonicalize_move_type(&wire.asset_coin_type),
            settlement_coin_type: canonicalize_move_type(&wire.settlement_coin_type),
            call_coin_type: if option_coin.is_empty() {
                String::new()
            } else {
                canonicalize_move_type(&option_coin)
            },
            is_put: wire.option_kind == "put",
            strike: wire
                .strike_raw
                .parse::<u128>()
                .with_context(|| format!("parsing strike_raw {:?}", wire.strike_raw))?,
            strike_scale: wire.strike_scale,
            expiry_ms: wire.expiry_ms.max(0) as u64,
        };
        self.cache.write().insert(id, pricing.clone());
        Ok(Some(pricing))
    }

    /// Open RFQ auctions from `GET /rfqs?status=open` — the on-chain
    /// bidder's discovery poll (vault-implementation-guide doc 05 §3.1).
    /// Never cached: deadlines and best bids move with every block.
    pub async fn open_rfqs(&self) -> Result<Vec<OpenRfq>> {
        self.open_rfqs_kind("call").await
    }

    /// Open cash-secured-put auctions from `GET /rfqs?status=open&kind=put` —
    /// the put on-chain bidder's discovery poll. `amount` is the put notional
    /// (underlying units); the bidder still escrows premium, not collateral.
    pub async fn open_put_rfqs(&self) -> Result<Vec<OpenRfq>> {
        self.open_rfqs_kind("put").await
    }

    async fn open_rfqs_kind(&self, kind: &str) -> Result<Vec<OpenRfq>> {
        let url = format!("{}/rfqs?status=open&kind={kind}", self.base_url);
        let wire: RfqsWire = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("GET {url} returned an error status"))?
            .json()
            .await
            .with_context(|| format!("decoding rfqs from {url}"))?;
        wire.rfqs
            .into_iter()
            .map(|r| {
                Ok(OpenRfq {
                    rfq_id: ObjectId::from_hex(&r.rfq_id)
                        .map_err(|e| anyhow::anyhow!("rfq_id {}: {e}", r.rfq_id))?,
                    bucket_id: ObjectId::from_hex(&r.bucket_id)
                        .map_err(|e| anyhow::anyhow!("bucket_id {}: {e}", r.bucket_id))?,
                    origin: r.origin,
                    amount: r
                        .amount_raw
                        .parse()
                        .with_context(|| format!("parsing amount_raw {:?}", r.amount_raw))?,
                    reserve_premium: r.reserve_premium_raw.parse().with_context(|| {
                        format!("parsing reserve_premium_raw {:?}", r.reserve_premium_raw)
                    })?,
                    deadline_ms: r.deadline_ms.max(0) as u64,
                })
            })
            .collect()
    }

    /// All buckets with a live DeepBook venue, fresh from `GET /buckets`
    /// (never cached — `tradeable` flips with the clock and pool creation;
    /// SO-158).
    pub async fn tradeable_buckets(&self) -> Result<Vec<TradeableBucket>> {
        let url = format!("{}/buckets", self.base_url);
        let wire: BucketsWire = observability::client::instrumented("api-service", "GET /buckets", |h| {
            self.http.get(&url).headers(h).send()
        })
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url} returned an error status"))?
        .json()
        .await
        .with_context(|| format!("decoding buckets from {url}"))?;

        let mut out = Vec::new();
        for series in wire.series {
            for b in series.buckets {
                if !b.tradeable {
                    continue;
                }
                let Some(pool_id) = b.deepbook_pool_id else { continue };
                out.push(TradeableBucket {
                    bucket_id: ObjectId::from_hex(&b.bucket_id)
                        .map_err(|e| anyhow::anyhow!("bucket_id {}: {e}", b.bucket_id))?,
                    pool_id,
                    call_coin_type: canonicalize_move_type(&b.call_coin_type),
                    asset_coin_type: canonicalize_move_type(&series.asset_coin_type),
                    settlement_coin_type: canonicalize_move_type(&series.settlement_coin_type),
                    asset_decimals: series.asset_decimals,
                    settlement_decimals: series.settlement_decimals,
                    strike_raw: b
                        .strike_raw
                        .parse::<u128>()
                        .with_context(|| format!("parsing strike_raw {:?}", b.strike_raw))?,
                    strike_scale: b.strike_scale,
                    expiry_ms: series.expiry_ms.max(0) as u64,
                    invalidated: b.invalidated,
                });
            }
        }
        Ok(out)
    }

    /// Vault ids with deposits paused on-chain, fresh from `GET /vaults`.
    /// The mm-bot treats a paused vault as decommissioned (hard cutover)
    /// and skips its RFQ and swap auctions.
    pub async fn paused_vault_ids(&self) -> Result<HashSet<ObjectId>> {
        let url = format!("{}/vaults", self.base_url);
        let wire: VaultsWire = observability::client::instrumented("api-service", "GET /vaults", |h| {
            self.http.get(&url).headers(h).send()
        })
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url} returned an error status"))?
        .json()
        .await
        .with_context(|| format!("decoding vaults from {url}"))?;

        wire.vaults
            .into_iter()
            .filter(|v| v.deposits_paused)
            .map(|v| {
                ObjectId::from_hex(&v.vault_id)
                    .map_err(|e| anyhow::anyhow!("vault_id {}: {e}", v.vault_id))
            })
            .collect()
    }
}

/// One bucket with a live DeepBook venue, flattened from `GET /buckets`
/// (SO-158). Only buckets api-service marks `tradeable` are returned —
/// pool exists, not cleaned, not expired.
#[derive(Clone, Debug)]
pub struct TradeableBucket {
    pub bucket_id: ObjectId,
    pub pool_id: String,
    pub call_coin_type: String,
    pub asset_coin_type: String,
    pub settlement_coin_type: String,
    pub asset_decimals: Option<u8>,
    pub settlement_decimals: Option<u8>,
    pub strike_raw: u128,
    pub strike_scale: u8,
    pub expiry_ms: u64,
    pub invalidated: bool,
}

/// One open auction from `GET /rfqs?status=open`, trimmed to discovery
/// fields. Live bid state (best premium / deadline extensions /
/// min-increment) is read from the auction object itself — the indexer
/// may lag bids.
#[derive(Clone, Debug)]
pub struct OpenRfq {
    pub rfq_id: ObjectId,
    pub bucket_id: ObjectId,
    /// Vault id (coupled auctions) or seller-address-as-id, hex.
    pub origin: String,
    /// Underlying escrowed in the slice.
    pub amount: u64,
    pub reserve_premium: u64,
    pub deadline_ms: u64,
}

#[derive(Deserialize)]
struct RfqsWire {
    rfqs: Vec<RfqWire>,
}

#[derive(Deserialize)]
struct VaultsWire {
    vaults: Vec<VaultStatusWire>,
}

#[derive(Deserialize)]
struct VaultStatusWire {
    vault_id: String,
    deposits_paused: bool,
}

#[derive(Deserialize)]
struct RfqWire {
    rfq_id: String,
    bucket_id: String,
    origin: String,
    amount_raw: String,
    reserve_premium_raw: String,
    deadline_ms: i64,
}

#[derive(Deserialize)]
struct BucketsWire {
    series: Vec<SeriesWire>,
}

#[derive(Deserialize)]
struct SeriesWire {
    asset_coin_type: String,
    settlement_coin_type: String,
    asset_decimals: Option<u8>,
    settlement_decimals: Option<u8>,
    expiry_ms: i64,
    buckets: Vec<SeriesBucketWire>,
}

#[derive(Deserialize)]
struct SeriesBucketWire {
    bucket_id: String,
    call_coin_type: String,
    strike_raw: String,
    strike_scale: u8,
    invalidated: bool,
    // serde defaults keep this client compatible with an api-service that
    // predates SO-153.
    #[serde(default)]
    deepbook_pool_id: Option<String>,
    #[serde(default)]
    tradeable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_canonicalizes_bucket_detail() {
        // api-service emits `0x`-canonical types already; the client must also
        // tolerate (and canonicalize) the bare `TypeName` form defensively.
        let raw = r#"{
            "bucket_id": "0x0a",
            "asset_coin_type": "9b72::twal::TWAL",
            "settlement_coin_type": "0x9b72::tusdc::TUSDC",
            "strike": 0.0315,
            "strike_raw": "31473",
            "strike_scale": 9,
            "expiry_ms": 1781740800000,
            "total_written_raw": "0"
        }"#;
        let wire: BucketDetailWire = serde_json::from_str(raw).unwrap();
        let p = BucketPricing {
            asset_coin_type: canonicalize_move_type(&wire.asset_coin_type),
            settlement_coin_type: canonicalize_move_type(&wire.settlement_coin_type),
            call_coin_type: wire.call_coin_type.clone(), // absent pre-C2 → empty
            is_put: wire.option_kind == "put",
            strike: wire.strike_raw.parse().unwrap(),
            strike_scale: wire.strike_scale,
            expiry_ms: wire.expiry_ms.max(0) as u64,
        };
        assert!(p.call_coin_type.is_empty());
        assert!(!p.is_put);
        // ceil collateral: 100 units × 31473 / 1e9 = 0.0031473 → 1.
        assert_eq!(p.put_collateral(100), 1);
        assert_eq!(
            p.asset_coin_type,
            "0x0000000000000000000000000000000000000000000000000000000000009b72::twal::TWAL"
        );
        assert_eq!(p.strike, 31_473);
        assert_eq!(p.strike_scale, 9);
        assert_eq!(p.expiry_ms, 1_781_740_800_000);
    }
}
