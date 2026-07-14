//! Minimal solana-api-service client — the bot-side analog of
//! `crates/api-service-client`'s `bucket_pricing` / `paused_vault_ids`
//! (kept local per the port plan: no new shared crate).
//!
//! Two reads:
//! - `GET /buckets/:bucket_id` — a bucket's true pricing inputs (strike,
//!   strike_scale, expiry, mints, option kind) by address. The bot resolves
//!   every RFQ / auction bucket here so a spoofed or buggy upstream can't
//!   hand it fake pricing inputs. Immutable fields, so results are cached.
//! - `GET /vaults` — the paused-vault filter for the auction bidders
//!   (`deposits_paused` is the Solana vault's pause flag).

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Context, Result};
use parking_lot::RwLock;
use serde::Deserialize;
use tracing::debug;

/// The pricing-relevant subset of `GET /buckets/:bucket_id`
/// (`BucketDetailDto` in solana-api-service). Mints are base58 strings,
/// compared byte-exact.
#[derive(Clone, Debug)]
pub struct BucketPricing {
    pub asset_mint: String,
    pub settlement_mint: String,
    /// SPL mint of the bucket's fungible option token.
    pub option_mint: String,
    /// True when this bucket is a cash-secured put (`option_kind == "put"`).
    pub is_put: bool,
    pub strike: u128,
    pub strike_scale: u8,
    pub expiry_ms: u64,
}

#[derive(Deserialize)]
struct BucketDetailWire {
    asset_mint: String,
    settlement_mint: String,
    option_mint: String,
    #[serde(default)]
    option_kind: String,
    strike_raw: String,
    strike_scale: u8,
    expiry_ms: i64,
}

#[derive(Deserialize)]
struct VaultsWire {
    vaults: Vec<VaultWire>,
}

#[derive(Deserialize)]
struct VaultWire {
    vault_id: String,
    deposits_paused: bool,
}

/// Resolves bucket pricing inputs from solana-api-service, caching
/// immutable results.
pub struct ApiClient {
    http: reqwest::Client,
    base_url: String,
    cache: RwLock<HashMap<String, BucketPricing>>,
}

impl ApiClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// One bucket's pricing inputs by base58 address; `None` when the
    /// api-service doesn't know it (or it's already cleaned).
    pub async fn bucket_pricing(&self, bucket_id: &str) -> Result<Option<BucketPricing>> {
        if let Some(hit) = self.cache.read().get(bucket_id).cloned() {
            return Ok(Some(hit));
        }

        let url = format!("{}/buckets/{bucket_id}", self.base_url);
        let resp = observability::client::instrumented(
            "solana-api-service",
            "GET /buckets/:id",
            |h| self.http.get(&url).headers(h).send(),
        )
        .await
        .with_context(|| format!("GET {url}"))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            debug!(bucket = %bucket_id, "solana-api-service: bucket not found");
            return Ok(None);
        }
        let resp = resp.error_for_status().with_context(|| format!("GET {url}"))?;
        let wire: BucketDetailWire = resp
            .json()
            .await
            .with_context(|| format!("decoding {url}"))?;
        let pricing = BucketPricing {
            asset_mint: wire.asset_mint,
            settlement_mint: wire.settlement_mint,
            option_mint: wire.option_mint,
            is_put: wire.option_kind == "put",
            strike: wire
                .strike_raw
                .parse::<u128>()
                .with_context(|| format!("parsing strike_raw {:?}", wire.strike_raw))?,
            strike_scale: wire.strike_scale,
            expiry_ms: wire.expiry_ms.max(0) as u64,
        };
        self.cache
            .write()
            .insert(bucket_id.to_string(), pricing.clone());
        Ok(Some(pricing))
    }

    /// Ids (base58) of vaults whose deposits are paused — the auction
    /// bidders drop any auction created by one of these before bidding.
    /// Never cached: the pause flag is an operator action.
    pub async fn paused_vault_ids(&self) -> Result<HashSet<String>> {
        let url = format!("{}/vaults", self.base_url);
        let wire: VaultsWire = observability::client::instrumented(
            "solana-api-service",
            "GET /vaults",
            |h| self.http.get(&url).headers(h).send(),
        )
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?
        .json()
        .await
        .map_err(|e| anyhow!("decoding {url}: {e}"))?;
        Ok(wire
            .vaults
            .into_iter()
            .filter(|v| v.deposits_paused)
            .map(|v| v.vault_id)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_wire_decodes_and_maps_put_kind() {
        let wire: BucketDetailWire = serde_json::from_str(
            r#"{
                "bucket_id": "bkt111",
                "asset_symbol": "TBTC",
                "asset_mint": "So11111111111111111111111111111111111111112",
                "settlement_mint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "option_mint": "opt111",
                "option_kind": "put",
                "strike_raw": "340282366920938463463374607431768211455",
                "strike_scale": 6,
                "expiry_ms": 1760000000000,
                "tradeable": true
            }"#,
        )
        .unwrap();
        assert_eq!(wire.option_kind, "put");
        assert_eq!(wire.strike_raw.parse::<u128>().unwrap(), u128::MAX);
        assert_eq!(wire.expiry_ms, 1_760_000_000_000);
    }

    #[test]
    fn vaults_wire_filters_paused() {
        let wire: VaultsWire = serde_json::from_str(
            r#"{"vaults":[
                {"vault_id":"v1","deposits_paused":false,"round":1},
                {"vault_id":"v2","deposits_paused":true}
            ]}"#,
        )
        .unwrap();
        let paused: HashSet<String> = wire
            .vaults
            .into_iter()
            .filter(|v| v.deposits_paused)
            .map(|v| v.vault_id)
            .collect();
        assert!(paused.contains("v2"));
        assert!(!paused.contains("v1"));
    }
}
