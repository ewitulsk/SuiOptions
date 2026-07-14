//! Test-token overlay (non-mainnet-beta networks).
//!
//! Instead of persisting the `solana-deployments.json` test tokens into
//! Postgres, we derive them once at boot and merge them into the `/tokens`
//! response at read time (see `handlers::tokens`). This keeps the DB to
//! durable, operator-managed tokens only, and the overlay always tracks
//! `solana-deployments.json` exactly.
//!
//! `decimals`/`pyth_feed_id` come from the deployment's `token_info` block
//! (the pricing source of truth) falling back to the testToken record;
//! `name`/`logo` come from the `[seed_meta.<TICKER>]` config map. Built only
//! when the network is not mainnet-beta.

use std::collections::BTreeMap;

use solana_deployments::SolanaNetworkDeployment;
use solana_token_info_client::SupportedToken;
use tracing::info;

use crate::config::SeedMeta;

/// Build the read-time overlay. Returns empty when `enabled` is false
/// (mainnet-beta) or the deployment has no testTokens.
pub fn build(
    dep: &SolanaNetworkDeployment,
    seed_meta: &BTreeMap<String, SeedMeta>,
    enabled: bool,
) -> Vec<SupportedToken> {
    if !enabled {
        return Vec::new();
    }
    let Some(tt) = dep.program_info.test_tokens.as_ref() else {
        return Vec::new();
    };

    let overlay: Vec<SupportedToken> = tt
        .iter()
        .map(|(symbol, t)| {
            let spec = dep.token_info.get(&symbol.to_ascii_uppercase());
            let decimals = spec.map(|s| s.decimals).unwrap_or(t.decimals);
            let pyth_feed_id = spec.and_then(|s| s.pyth_feed_id.clone());
            // The `config` crate lowercases all TOML keys, so `[seed_meta.TBTC]`
            // arrives as `tbtc`. Look up case-insensitively (lowercase first).
            let meta = seed_meta
                .get(&symbol.to_ascii_lowercase())
                .or_else(|| seed_meta.get(symbol))
                .or_else(|| seed_meta.get(&symbol.to_ascii_uppercase()));
            SupportedToken {
                mint: t.mint.clone(),
                ticker: symbol.clone(),
                name: meta
                    .and_then(|m| m.name.clone())
                    .unwrap_or_else(|| symbol.clone()),
                logo_uri: meta.and_then(|m| m.logo_uri.clone()),
                decimals,
                pyth_feed_id,
                enabled: true,
            }
        })
        .collect();

    info!(count = overlay.len(), "built test-token overlay");
    overlay
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_deployments::{ProgramInfo, TestToken, TokenSpec};

    const MINT_TBTC: &str = "So11111111111111111111111111111111111111112";
    const MINT_TUSDC: &str = "11111111111111111111111111111111";
    const PYTH_BTC: &str = "e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43";

    fn dep() -> SolanaNetworkDeployment {
        let mut test_tokens = BTreeMap::new();
        test_tokens.insert(
            "TBTC".to_string(),
            TestToken {
                mint: MINT_TBTC.into(),
                decimals: 9, // deliberately wrong; token_info (8) must win
                mint_authority: MINT_TUSDC.into(),
            },
        );
        test_tokens.insert(
            "TUSDC".to_string(),
            TestToken {
                mint: MINT_TUSDC.into(),
                decimals: 6,
                mint_authority: MINT_TUSDC.into(),
            },
        );
        let mut token_info = BTreeMap::new();
        token_info.insert(
            "TBTC".to_string(),
            TokenSpec {
                mint: MINT_TBTC.into(),
                decimals: 8,
                pyth_feed_id: Some(PYTH_BTC.into()),
            },
        );
        SolanaNetworkDeployment {
            program_info: ProgramInfo {
                options_core_program_id: "6KeiQVrkr7uxW1LKhZGpjg7yaYVrz4AKyGaD7Dgnef1t".into(),
                auction_venue_program_id: "8cvpWnJaQ4kTEPypwrZvBPzEM4R7FbivgybXBm2ahvKk".into(),
                options_vault_program_id: "ELxbfwPUPJ4U1SnvWZJpLxdCRbgMiBpgQmdRizNWYcXe".into(),
                config_pda: MINT_TUSDC.into(),
                treasury_pda: MINT_TUSDC.into(),
                admin: MINT_TUSDC.into(),
                network: "devnet".into(),
                deployed_at: "2026-07-11T00:00:00Z".into(),
                initialize_signature: None,
                test_tokens: Some(test_tokens),
            },
            token_info,
        }
    }

    fn seed(entries: &[(&str, &str, Option<&str>)]) -> BTreeMap<String, SeedMeta> {
        entries
            .iter()
            .map(|(k, name, logo)| {
                (
                    k.to_string(),
                    SeedMeta {
                        name: Some(name.to_string()),
                        logo_uri: logo.map(|s| s.to_string()),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn builds_overlay_from_test_tokens() {
        // Lowercase seed keys — the `config` crate lowercases TOML keys.
        let meta = seed(&[("tbtc", "Test Bitcoin", Some("https://logo/btc.png"))]);
        let overlay = build(&dep(), &meta, true);
        assert_eq!(overlay.len(), 2);

        let tbtc = overlay.iter().find(|t| t.ticker == "TBTC").unwrap();
        assert_eq!(tbtc.mint, MINT_TBTC);
        assert_eq!(tbtc.name, "Test Bitcoin");
        assert_eq!(tbtc.logo_uri.as_deref(), Some("https://logo/btc.png"));
        // decimals + pyth feed come from token_info, not the testToken record.
        assert_eq!(tbtc.decimals, 8);
        assert_eq!(tbtc.pyth_feed_id.as_deref(), Some(PYTH_BTC));
        assert!(tbtc.enabled);

        // No token_info / seed_meta entry: falls back to testToken decimals,
        // ticker as name, no logo, no feed.
        let tusdc = overlay.iter().find(|t| t.ticker == "TUSDC").unwrap();
        assert_eq!(tusdc.mint, MINT_TUSDC);
        assert_eq!(tusdc.name, "TUSDC");
        assert_eq!(tusdc.decimals, 6);
        assert!(tusdc.logo_uri.is_none());
        assert!(tusdc.pyth_feed_id.is_none());
    }

    #[test]
    fn seed_meta_lookup_is_case_insensitive() {
        // Uppercase keys (hand-built map) must also resolve.
        let meta = seed(&[("TUSDC", "Test USDC", None)]);
        let overlay = build(&dep(), &meta, true);
        let tusdc = overlay.iter().find(|t| t.ticker == "TUSDC").unwrap();
        assert_eq!(tusdc.name, "Test USDC");
    }

    #[test]
    fn disabled_or_missing_test_tokens_yield_empty() {
        assert!(build(&dep(), &BTreeMap::new(), false).is_empty());

        let mut d = dep();
        d.program_info.test_tokens = None;
        assert!(build(&d, &BTreeMap::new(), true).is_empty());
    }
}
