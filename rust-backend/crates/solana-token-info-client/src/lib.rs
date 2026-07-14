//! Shared client for the **solana-token-info service**
//! (`services/solana-token-info`).
//!
//! solana-token-info is the single source of truth for the Solana
//! supported-token catalog and the deployed programs' ids. It is the ONLY
//! service permitted to read `solana-deployments.json`; every other Solana
//! service and tool reads from it through this crate instead.
//!
//! ## Design
//!
//! The Solana twin of `crates/token-info-client`: a consumer builds a
//! [`TokenInfoClient`] from the service's public base URL, calls
//! [`TokenInfoClient::fetch`] (or
//! [`TokenInfoClient::fetch_blocking_until_ready`]) once at boot, and holds
//! the returned [`Snapshot`]. All ids are base58 `String`s — no solana-sdk
//! dep, so the crate is importable from both the main workspace and the
//! standalone Solana service workspaces.
//!
//! ## Hard cutover
//!
//! There is no `solana-deployments.json` fallback. If solana-token-info is
//! unreachable at boot, the fetch errors and the caller is expected to
//! propagate it and crash. [`TokenInfoClient::fetch_blocking_until_ready`]
//! retries for a bounded window (to tolerate the service still warming up)
//! and then returns the error.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

pub use solana_deployments::{ProgramInfo, TestToken, TokenSpec};

/// One supported-token catalog entry as served by `GET /tokens`.
///
/// `mint` is the SPL mint address (base58) — the token's identity,
/// compared byte-exact. `ticker` is the symbol (`TBTC`, `USDC`) consumers
/// look tokens up by.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupportedToken {
    pub mint: String,
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
    /// Typed Pyth feed id (feed ids are chain-agnostic 64-hex). Errors if
    /// absent or malformed — mirrors the Sui client's helper.
    pub fn pyth_feed(&self) -> Result<protocol_types::PriceFeedId> {
        let raw = self
            .pyth_feed_id
            .as_deref()
            .ok_or_else(|| {
                anyhow!("token {} has no pyth_feed_id in solana-token-info", self.ticker)
            })?;
        protocol_types::PriceFeedId::from_hex(raw)
    }
}

/// An immutable view of solana-token-info's state at fetch time: the
/// `program_info` block for the configured env plus the full token catalog.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub program_info: ProgramInfo,
    pub tokens: Vec<SupportedToken>,
}

impl Snapshot {
    // --- program-info accessors (base58 strings) ----------------------------

    pub fn core_program(&self) -> &str {
        &self.program_info.options_core_program_id
    }
    pub fn venue_program(&self) -> &str {
        &self.program_info.auction_venue_program_id
    }
    pub fn vault_program(&self) -> &str {
        &self.program_info.options_vault_program_id
    }
    /// options_core Config PDA — the quote `protocol_id`.
    pub fn config_pda(&self) -> &str {
        &self.program_info.config_pda
    }
    pub fn treasury_pda(&self) -> &str {
        &self.program_info.treasury_pda
    }
    pub fn admin(&self) -> &str {
        &self.program_info.admin
    }

    /// Solana cluster (`devnet` / `testnet` / `mainnet-beta`) this
    /// deployment targets. Consumers that need an RPC URL derive it from
    /// here instead of carrying their own `network` config.
    pub fn network(&self) -> &str {
        &self.program_info.network
    }

    // --- faucet accessors (testTokens passthrough) --------------------------
    //
    // Test mints are a non-mainnet deploy artifact served from
    // `/program-info`. Use these ONLY for faucet/mint operations — mints,
    // decimals and Pyth feeds for every consumer come from the `/tokens`
    // catalog accessors below.

    /// Entire `testTokens` block. Non-mainnet only; errors on mainnet-beta
    /// where it's absent.
    pub fn test_tokens(&self) -> Result<&std::collections::BTreeMap<String, TestToken>> {
        self.program_info
            .test_tokens
            .as_ref()
            .ok_or_else(|| anyhow!("no testTokens section served by solana-token-info"))
    }

    /// Case-insensitive test-token lookup (mint + mint authority). For
    /// minting test tokens only — NOT the general token source.
    pub fn faucet_token(&self, symbol: &str) -> Result<&TestToken> {
        let tokens = self.test_tokens()?;
        let upper = symbol.to_ascii_uppercase();
        tokens.get(&upper).ok_or_else(|| {
            anyhow!(
                "no test token named {symbol} (have: {:?})",
                tokens.keys().collect::<Vec<_>>()
            )
        })
    }

    // --- catalog accessors (/tokens) -----------------------------------------
    //
    // The single token surface for every consumer: mint, decimals, Pyth
    // feed, name, logo. On dev/staging this includes the test tokens
    // (overlaid by solana-token-info); on mainnet it's the operator-managed
    // catalog.

    pub fn tokens(&self) -> &[SupportedToken] {
        &self.tokens
    }

    /// Look up a catalog entry by ticker (case-insensitive).
    pub fn token_spec(&self, symbol: &str) -> Result<&SupportedToken> {
        let upper = symbol.to_ascii_uppercase();
        self.tokens
            .iter()
            .find(|t| t.ticker.to_ascii_uppercase() == upper)
            .ok_or_else(|| {
                anyhow!(
                    "no solana-token-info catalog entry for {symbol} (have: {:?})",
                    self.tokens.iter().map(|t| &t.ticker).collect::<Vec<_>>()
                )
            })
    }

    /// Look up a catalog entry by mint address. Byte-exact comparison —
    /// base58 needs no normalization.
    pub fn token_by_mint(&self, mint: &str) -> Option<&SupportedToken> {
        self.tokens.iter().find(|t| t.mint == mint)
    }
}

/// Async client over solana-token-info's public read API.
#[derive(Debug, Clone)]
pub struct TokenInfoClient {
    base_url: String,
    http: reqwest::Client,
}

impl TokenInfoClient {
    /// `base_url` is the service's public base, e.g.
    /// `http://solana-token-info:9005` or
    /// `https://api.example.com/prod/solana-token-info`. A trailing slash
    /// is fine.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// Fetch `program_info` + the token catalog once. Errors if
    /// solana-token-info is unreachable or returns non-2xx — the caller is
    /// expected to crash.
    pub async fn fetch(&self) -> Result<Snapshot> {
        let program_info = self
            .get_json::<ProgramInfo>("/program-info", "GET /program-info")
            .await
            .context("fetching /program-info from solana-token-info")?;
        let tokens = self
            .get_json::<Vec<SupportedToken>>("/tokens", "GET /tokens")
            .await
            .context("fetching /tokens from solana-token-info")?;
        info!(
            base = %self.base_url,
            network = %program_info.network,
            tokens = tokens.len(),
            "fetched snapshot from solana-token-info"
        );
        Ok(Snapshot {
            program_info,
            tokens,
        })
    }

    /// Like [`fetch`](Self::fetch) but retries on failure for up to
    /// `max_attempts` with `delay` between tries — solana-token-info may
    /// still be booting. After the window is exhausted the last error is
    /// returned so the caller crashes. There is no `solana-deployments.json`
    /// fallback.
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
                        "solana-token-info not ready; retrying"
                    );
                    last_err = Some(e);
                    if attempt < max_attempts {
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("solana-token-info unreachable"))).with_context(
            || {
                format!(
                    "solana-token-info at {} unreachable after {max_attempts} attempts",
                    self.base_url
                )
            },
        )
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        op: &'static str,
    ) -> Result<T> {
        let url = format!("{}{}", self.base_url, path);
        let resp = observability::client::instrumented("solana-token-info", op, |h| {
            self.http.get(&url).headers(h).send()
        })
        .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("GET {url} -> {status}: {body}"));
        }
        resp.json::<T>().await.with_context(|| format!("decoding {url}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const MINT_TBTC: &str = "So11111111111111111111111111111111111111112";
    const MINT_TUSDC: &str = "11111111111111111111111111111111";

    fn tok(ticker: &str, mint: &str, pyth: Option<&str>) -> SupportedToken {
        SupportedToken {
            mint: mint.into(),
            ticker: ticker.into(),
            name: ticker.into(),
            logo_uri: None,
            decimals: 8,
            pyth_feed_id: pyth.map(|s| s.into()),
            enabled: true,
        }
    }

    fn snap() -> Snapshot {
        let mut test_tokens = BTreeMap::new();
        test_tokens.insert(
            "TBTC".to_string(),
            TestToken {
                mint: MINT_TBTC.into(),
                decimals: 8,
                mint_authority: MINT_TUSDC.into(),
            },
        );
        Snapshot {
            program_info: ProgramInfo {
                options_core_program_id: "6KeiQVrkr7uxW1LKhZGpjg7yaYVrz4AKyGaD7Dgnef1t".into(),
                auction_venue_program_id: "8cvpWnJaQ4kTEPypwrZvBPzEM4R7FbivgybXBm2ahvKk".into(),
                options_vault_program_id: "ELxbfwPUPJ4U1SnvWZJpLxdCRbgMiBpgQmdRizNWYcXe".into(),
                config_pda: "cfg".into(),
                treasury_pda: "treas".into(),
                admin: "adm".into(),
                network: "devnet".into(),
                deployed_at: "".into(),
                initialize_signature: None,
                test_tokens: Some(test_tokens),
            },
            tokens: vec![
                tok(
                    "TBTC",
                    MINT_TBTC,
                    Some("e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43"),
                ),
                tok("TUSDC", MINT_TUSDC, None),
            ],
        }
    }

    #[test]
    fn accessors_pass_through_program_info() {
        let s = snap();
        assert_eq!(s.core_program(), "6KeiQVrkr7uxW1LKhZGpjg7yaYVrz4AKyGaD7Dgnef1t");
        assert_eq!(s.venue_program(), "8cvpWnJaQ4kTEPypwrZvBPzEM4R7FbivgybXBm2ahvKk");
        assert_eq!(s.vault_program(), "ELxbfwPUPJ4U1SnvWZJpLxdCRbgMiBpgQmdRizNWYcXe");
        assert_eq!(s.config_pda(), "cfg");
        assert_eq!(s.treasury_pda(), "treas");
        assert_eq!(s.admin(), "adm");
        assert_eq!(s.network(), "devnet");
    }

    #[test]
    fn token_lookup_by_ticker_is_case_insensitive() {
        let s = snap();
        assert_eq!(s.token_spec("tbtc").unwrap().mint, MINT_TBTC);
        assert!(s.token_spec("TBTC").unwrap().pyth_feed().is_ok());
        assert!(s.token_spec("tusdc").unwrap().pyth_feed().is_err());
        assert!(s.token_spec("nope").is_err());
    }

    #[test]
    fn token_lookup_by_mint_is_byte_exact() {
        let s = snap();
        assert_eq!(s.token_by_mint(MINT_TBTC).unwrap().ticker, "TBTC");
        // No normalization on base58: near-miss strings don't match.
        assert!(s.token_by_mint("so11111111111111111111111111111111111111112").is_none());
    }

    #[test]
    fn faucet_token_is_case_insensitive_and_mainnet_errors() {
        let mut s = snap();
        assert_eq!(s.faucet_token("tbtc").unwrap().mint, MINT_TBTC);
        assert!(s.faucet_token("nope").is_err());
        s.program_info.test_tokens = None;
        assert!(s.test_tokens().is_err());
        assert!(s.faucet_token("TBTC").is_err());
    }

    #[test]
    fn supported_token_serde_defaults() {
        // Minimal wire form: logo_uri / pyth_feed_id default None, enabled
        // defaults true.
        let t: SupportedToken = serde_json::from_str(
            r#"{ "mint": "m", "ticker": "TBTC", "name": "Test BTC", "decimals": 8 }"#,
        )
        .unwrap();
        assert!(t.logo_uri.is_none());
        assert!(t.pyth_feed_id.is_none());
        assert!(t.enabled);

        // Round-trip keeps explicit values.
        let j = serde_json::to_string(&t).unwrap();
        let back: SupportedToken = serde_json::from_str(&j).unwrap();
        assert_eq!(back.ticker, "TBTC");
        assert!(back.enabled);

        let disabled: SupportedToken = serde_json::from_str(
            r#"{ "mint": "m", "ticker": "T", "name": "T", "decimals": 6, "enabled": false }"#,
        )
        .unwrap();
        assert!(!disabled.enabled);
    }
}
