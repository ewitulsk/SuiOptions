//! Shared client for the **token-info service** (`services/token-info`).
//!
//! token-info is the single source of truth for the supported-token catalog
//! and the protocol's on-chain ids. It is the ONLY service permitted to read
//! `deployments.json`; every other service and tool reads from it through
//! this crate instead.
//!
//! ## Design
//!
//! A consumer builds a [`TokenInfoClient`] from the service's public base URL,
//! calls [`TokenInfoClient::fetch`] (or [`TokenInfoClient::fetch_blocking_until_ready`])
//! once at boot, and holds the returned [`Snapshot`]. The snapshot mirrors the
//! accessor surface of `deployments::NetworkDeployment` (`package()`,
//! `admin_cap()`, `protocol_config()`, `treasury()`, `deployer_address()`,
//! `protocol_id_bytes()`, `token_spec()`, `test_tokens()`, …) so call sites
//! that used `Deployments::load(path)?.for_env(env)?` swap to
//! `TokenInfoClient::new(url).fetch().await?` with minimal churn.
//!
//! ## Hard cutover
//!
//! There is no `deployments.json` fallback. If token-info is unreachable at
//! boot, the fetch errors and the caller is expected to propagate it and
//! crash. [`TokenInfoClient::fetch_blocking_until_ready`] retries for a bounded
//! window (to tolerate token-info still warming up) and then returns the error.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sui_types::base_types::{ObjectID, SuiAddress};
use tracing::{info, warn};

pub use deployments::{DeepBookInfo, PackageInfo, TestTokens, TokenInfo};

/// One supported-token catalog entry as served by `GET /tokens`.
///
/// `coin_type` is the fully-qualified Move type. `ticker` is the symbol
/// (`TBTC`, `USDC`) consumers look tokens up by. `decimals` and the optional
/// `pyth_feed_id` replace what `deployments.json::token_info` used to carry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupportedToken {
    pub coin_type: String,
    pub ticker: String,
    pub name: String,
    #[serde(default)]
    pub logo_uri: Option<String>,
    pub decimals: u8,
    #[serde(default)]
    pub pyth_feed_id: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl SupportedToken {
    /// Typed Pyth feed id. Errors if absent or malformed — mirrors
    /// `deployments::TokenSpec::pyth_feed`.
    pub fn pyth_feed(&self) -> Result<protocol_types::PriceFeedId> {
        let raw = self
            .pyth_feed_id
            .as_deref()
            .ok_or_else(|| anyhow!("token {} has no pyth_feed_id in token-info", self.ticker))?;
        protocol_types::PriceFeedId::from_hex(raw)
    }
}

/// An immutable view of token-info's state at fetch time: the protocol
/// `package_info` for the configured network plus the full token catalog.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub package_info: PackageInfo,
    pub tokens: Vec<SupportedToken>,
}

impl Snapshot {
    // --- NetworkDeployment-mirroring id accessors ---------------------------

    pub fn package(&self) -> Result<ObjectID> {
        ObjectID::from_str_checked(&self.package_info.package_id, "package_id")
    }
    pub fn admin_cap(&self) -> Result<ObjectID> {
        ObjectID::from_str_checked(&self.package_info.admin_cap_id, "admin_cap_id")
    }
    pub fn protocol_config(&self) -> Result<ObjectID> {
        ObjectID::from_str_checked(&self.package_info.protocol_config_id, "protocol_config_id")
    }
    pub fn treasury(&self) -> Result<ObjectID> {
        let id = self
            .package_info
            .treasury_id
            .as_deref()
            .ok_or_else(|| anyhow!("treasury_id missing from token-info package_info"))?;
        ObjectID::from_str_checked(id, "treasury_id")
    }
    pub fn deployer_address(&self) -> Result<SuiAddress> {
        use std::str::FromStr;
        SuiAddress::from_str(&self.package_info.deployer).context("parsing deployer")
    }

    /// Sui network slot (`testnet` / `mainnet` / `devnet`) this deployment
    /// targets. Consumers that need an RPC URL derive it from here instead of
    /// carrying their own `network` config.
    pub fn network(&self) -> &str {
        &self.package_info.network
    }

    /// Raw bytes of the AdminCap id — the domain separator the chain compares
    /// every `Quote.protocol_id` against. Mirrors
    /// `deployments::NetworkDeployment::protocol_id_bytes`.
    pub fn protocol_id_bytes(&self) -> Result<Vec<u8>> {
        Ok(self.admin_cap()?.into_bytes().to_vec())
    }

    /// DeepBook v3 deployment ids for this network (SO-151). `None` where
    /// DeepBook isn't deployed (devnet) — DeepBook-dependent features must
    /// degrade gracefully rather than crash.
    pub fn deepbook(&self) -> Option<&DeepBookInfo> {
        self.package_info.deepbook.as_ref()
    }

    // --- faucet accessors (testTokens passthrough) -------------------------
    //
    // Faucets are a testnet-only deploy artifact served from `/package-info`.
    // Use these ONLY for mint/fund operations — coin types, decimals and Pyth
    // feeds for every consumer come from the `/tokens` catalog accessors below.

    /// Entire `testTokens` block (faucets + coin types). Testnet/dev only;
    /// errors on mainnet where it's absent.
    pub fn test_tokens(&self) -> Result<&TestTokens> {
        self.package_info
            .test_tokens
            .as_ref()
            .ok_or_else(|| anyhow!("no testTokens section served by token-info"))
    }

    /// `Option`-flavored access to the testTokens block.
    pub fn maybe_test_tokens(&self) -> Option<&TestTokens> {
        self.package_info.test_tokens.as_ref()
    }

    /// Case-insensitive faucet lookup for a test token (faucet id + module
    /// path). For minting test tokens only — NOT the general token source.
    pub fn faucet_token(&self, symbol: &str) -> Result<&TokenInfo> {
        self.test_tokens()?.get(symbol)
    }

    // --- catalog accessors (/tokens) ---------------------------------------
    //
    // The single token surface for every consumer: coin type, decimals, Pyth
    // feed, name, logo. On dev/staging this includes the test tokens (overlaid
    // by token-info); on mainnet it's the operator-managed catalog.

    pub fn tokens(&self) -> &[SupportedToken] {
        &self.tokens
    }

    /// Look up a catalog entry by ticker (case-insensitive). Replaces
    /// `NetworkDeployment::token_spec`.
    pub fn token_spec(&self, symbol: &str) -> Result<&SupportedToken> {
        let upper = symbol.to_ascii_uppercase();
        self.tokens
            .iter()
            .find(|t| t.ticker.to_ascii_uppercase() == upper)
            .ok_or_else(|| {
                anyhow!(
                    "no token-info catalog entry for {symbol} (have: {:?})",
                    self.tokens.iter().map(|t| &t.ticker).collect::<Vec<_>>()
                )
            })
    }

    /// Look up a catalog entry by fully-qualified coin type.
    pub fn token_by_coin_type(&self, coin_type: &str) -> Option<&SupportedToken> {
        let want = normalize_coin_type(coin_type);
        self.tokens
            .iter()
            .find(|t| normalize_coin_type(&t.coin_type) == want)
    }
}

/// Async client over token-info's public read API.
#[derive(Debug, Clone)]
pub struct TokenInfoClient {
    base_url: String,
    http: reqwest::Client,
}

impl TokenInfoClient {
    /// `base_url` is the service's public base, e.g. `http://token-info:9005`
    /// or `https://api.example.com/prod/token-info`. A trailing slash is fine.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// Fetch `package_info` + the token catalog once. Errors if token-info is
    /// unreachable or returns non-2xx — the caller is expected to crash.
    pub async fn fetch(&self) -> Result<Snapshot> {
        let package_info = self
            .get_json::<PackageInfo>("/package-info")
            .await
            .context("fetching /package-info from token-info")?;
        let tokens = self
            .get_json::<Vec<SupportedToken>>("/tokens")
            .await
            .context("fetching /tokens from token-info")?;
        info!(
            base = %self.base_url,
            network = %package_info.network,
            tokens = tokens.len(),
            "fetched snapshot from token-info"
        );
        Ok(Snapshot {
            package_info,
            tokens,
        })
    }

    /// Like [`fetch`](Self::fetch) but retries on failure for up to
    /// `max_attempts` with `delay` between tries — token-info may still be
    /// booting. After the window is exhausted the last error is returned so
    /// the caller crashes. There is no `deployments.json` fallback.
    pub async fn fetch_blocking_until_ready(
        &self,
        max_attempts: u32,
        delay: Duration,
    ) -> Result<Snapshot> {
        let mut last_err = None;
        for attempt in 1..=max_attempts {
            match self.fetch().await {
                Ok(snap) => return Ok(snap),
                Err(e) => {
                    warn!(
                        base = %self.base_url,
                        attempt,
                        max_attempts,
                        error = %e,
                        "token-info not ready; retrying"
                    );
                    last_err = Some(e);
                    if attempt < max_attempts {
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("token-info unreachable")))
            .with_context(|| format!("token-info at {} unreachable after {max_attempts} attempts", self.base_url))
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("GET {url} -> {status}: {body}"));
        }
        resp.json::<T>().await.with_context(|| format!("decoding {url}"))
    }
}

/// Normalize a Move type string so address-format differences don't break
/// coin-type comparisons (mirrors the api-service catalog normalization):
/// strip `0x`, lowercase, left-pad the address to 64 hex chars.
fn normalize_coin_type(s: &str) -> String {
    let (addr, rest) = match s.split_once("::") {
        Some(parts) => parts,
        None => return s.to_string(),
    };
    let stripped = addr.strip_prefix("0x").unwrap_or(addr).to_ascii_lowercase();
    format!("{:0>64}::{}", stripped, rest)
}

/// Small helper so the accessors give a contextual error on a malformed id.
trait FromStrChecked: Sized {
    fn from_str_checked(s: &str, field: &str) -> Result<Self>;
}
impl FromStrChecked for ObjectID {
    fn from_str_checked(s: &str, field: &str) -> Result<Self> {
        use std::str::FromStr;
        ObjectID::from_str(s).with_context(|| format!("parsing {field}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(ticker: &str, coin: &str, pyth: Option<&str>) -> SupportedToken {
        SupportedToken {
            coin_type: coin.into(),
            ticker: ticker.into(),
            name: ticker.into(),
            logo_uri: None,
            decimals: 8,
            pyth_feed_id: pyth.map(|s| s.into()),
            enabled: true,
        }
    }

    fn snap() -> Snapshot {
        Snapshot {
            package_info: PackageInfo {
                package_id: "0x2".into(),
                admin_cap_id:
                    "0x3a094ab9d022f51ef18271e1226c32405df85b4fada60492383de59324b191c8".into(),
                protocol_config_id: "0x4".into(),
                upgrade_cap_id: "0x5".into(),
                treasury_id: Some("0x6".into()),
                publish_digest: "x".into(),
                init_digest: None,
                deployer: "0x7".into(),
                deployed_at: "".into(),
                network: "testnet".into(),
                test_tokens: None,
                deepbook: None,
            },
            tokens: vec![
                tok(
                    "TBTC",
                    "0xpkg::tbtc::TBTC",
                    Some("0xe62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43"),
                ),
                tok("TWAL", "0xpkg::twal::TWAL", None),
            ],
        }
    }

    #[test]
    fn protocol_id_is_admin_cap_bytes() {
        let s = snap();
        let bytes = s.protocol_id_bytes().unwrap();
        assert_eq!(bytes.len(), 32);
        assert_eq!(bytes[0], 0x3a);
        assert_eq!(bytes[31], 0xc8);
    }

    #[test]
    fn token_lookup_by_ticker_is_case_insensitive() {
        let s = snap();
        assert_eq!(s.token_spec("tbtc").unwrap().coin_type, "0xpkg::tbtc::TBTC");
        assert!(s.token_spec("TBTC").unwrap().pyth_feed().is_ok());
        assert!(s.token_spec("twal").unwrap().pyth_feed().is_err());
        assert!(s.token_spec("nope").is_err());
    }

    #[test]
    fn token_lookup_by_coin_type_normalizes_address() {
        let s = snap();
        // 0x-prefixed and bare forms collide on the same entry.
        assert!(s.token_by_coin_type("0xpkg::tbtc::TBTC").is_some());
        assert_eq!(
            s.token_by_coin_type("pkg::tbtc::TBTC").unwrap().ticker,
            "TBTC"
        );
    }

    #[test]
    fn supported_token_json_roundtrips() {
        let t = tok("TUSDC", "0xpkg::tusdc::TUSDC", None);
        let j = serde_json::to_string(&t).unwrap();
        let back: SupportedToken = serde_json::from_str(&j).unwrap();
        assert_eq!(back.ticker, "TUSDC");
        assert!(back.enabled);
    }
}
