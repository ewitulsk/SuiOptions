//! Coin-type → {symbol, decimals} lookup.
//!
//! Built once at startup by joining the two halves of the per-network
//! deployment record in `deployments.json`:
//!
//! - `package_info.testTokens.tokens` (testnet-only, has `coinType` +
//!   `decimals`) and
//! - `token_info` (every network; has `coinType` + `decimals`).
//!
//! Both halves agree on `coinType` for the same symbol, so we merge them
//! and key the resulting table by the fully-qualified Move type. That's
//! what every chain event carries in its `AssetType` field, so handlers
//! can resolve a bucket's `asset_type` / `settlement_type` to display
//! data in one hash lookup.

use std::collections::HashMap;

use anyhow::Result;
use tracing::{debug, info, warn};

use shared::deployments::Deployments;

#[derive(Clone, Debug)]
pub struct TokenMeta {
    pub symbol: String,
    pub decimals: u8,
}

#[derive(Default, Debug)]
pub struct TokenCatalog {
    by_coin_type: HashMap<String, TokenMeta>,
}

impl TokenCatalog {
    /// Build the catalog from the network slot in `deployments.json`.
    ///
    /// The merge is forgiving: a symbol present in only one of
    /// `testTokens` / `token_info` still ends up in the table.
    pub fn from_deployments(deployments: &Deployments, network: &str) -> Result<Self> {
        let dep = deployments.for_network(network)?;
        let mut by_coin_type: HashMap<String, TokenMeta> = HashMap::new();

        // testTokens (testnet only) — keyed by uppercase symbol.
        if let Some(tt) = dep.maybe_test_tokens() {
            for (symbol, info) in &tt.tokens {
                by_coin_type.insert(
                    normalize_coin_type(&info.coin_type),
                    TokenMeta {
                        symbol: symbol.clone(),
                        decimals: info.decimals,
                    },
                );
            }
        }

        // token_info — every network. If a coin_type was already present
        // from testTokens, prefer token_info's decimals (it's the source
        // of truth for pricing) but log if they disagree.
        for (symbol, spec) in &dep.token_info {
            let key = normalize_coin_type(&spec.coin_type);
            if let Some(existing) = by_coin_type.get(&key) {
                if existing.decimals != spec.decimals {
                    warn!(
                        coin_type = %spec.coin_type,
                        test_tokens = existing.decimals,
                        token_info = spec.decimals,
                        "decimals mismatch between testTokens and token_info; using token_info"
                    );
                }
            }
            by_coin_type.insert(
                key,
                TokenMeta {
                    symbol: symbol.clone(),
                    decimals: spec.decimals,
                },
            );
        }

        info!(
            network,
            tokens = by_coin_type.len(),
            "built token catalog"
        );
        for (coin_type, meta) in &by_coin_type {
            debug!(%coin_type, symbol = %meta.symbol, decimals = meta.decimals, "catalog entry");
        }

        Ok(Self { by_coin_type })
    }

    /// Look up by the Move type string carried in chain events. Returns
    /// `None` if the catalog has no entry — handlers should fall back to
    /// the raw string in that case rather than dropping the bucket.
    pub fn lookup(&self, coin_type: &str) -> Option<&TokenMeta> {
        self.by_coin_type.get(&normalize_coin_type(coin_type))
    }
}

/// Normalize a Move type string so address-format differences don't break
/// lookups. Chain events carry the canonical form (no `0x` prefix,
/// 64-hex-char address); `deployments.json` stores the convenient form
/// (`0x`-prefixed, possibly trimmed). We canonicalize to the chain form:
/// strip leading `0x`, lowercase, left-pad the address to 64 hex chars.
///
/// Returns the input unchanged if it doesn't contain `::` (defensive —
/// shouldn't happen for real coin types).
fn normalize_coin_type(s: &str) -> String {
    let (addr, rest) = match s.split_once("::") {
        Some(parts) => parts,
        None => return s.to_string(),
    };
    let stripped = addr.strip_prefix("0x").unwrap_or(addr).to_ascii_lowercase();
    let padded = format!("{:0>64}", stripped);
    format!("{}::{}", padded, rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::deployments::{NetworkDeployment, PackageInfo, TestTokens, TokenInfo, TokenSpec};
    use std::collections::BTreeMap;

    fn fixture_deployments() -> Deployments {
        let mut tokens = BTreeMap::new();
        tokens.insert(
            "TBTC".to_string(),
            TokenInfo {
                coin_type: "0xpkg::tbtc::TBTC".into(),
                faucet_id: "0x1".into(),
                decimals: 8,
            },
        );
        tokens.insert(
            "TUSDC".to_string(),
            TokenInfo {
                coin_type: "0xpkg::tusdc::TUSDC".into(),
                faucet_id: "0x2".into(),
                decimals: 6,
            },
        );
        let mut token_info = BTreeMap::new();
        token_info.insert(
            "TBTC".into(),
            TokenSpec {
                coin_type: "0xpkg::tbtc::TBTC".into(),
                decimals: 8,
                pyth_feed_id: None,
            },
        );
        Deployments {
            mainnet: None,
            devnet: None,
            testnet: Some(NetworkDeployment {
                package_info: PackageInfo {
                    package_id: "0xp".into(),
                    admin_cap_id: "0xa".into(),
                    protocol_config_id: "0xc".into(),
                    upgrade_cap_id: "0xu".into(),
                    treasury_id: None,
                    publish_digest: "x".into(),
                    init_digest: None,
                    deployer: "0xd".into(),
                    deployed_at: "".into(),
                    network: "testnet".into(),
                    test_tokens: Some(TestTokens {
                        package_id: "0xtp".into(),
                        upgrade_cap_id: "0xtu".into(),
                        publish_digest: "y".into(),
                        deployed_at: "".into(),
                        tokens,
                    }),
                },
                token_info,
            }),
        }
    }

    #[test]
    fn merges_test_tokens_and_token_info() {
        let cat = TokenCatalog::from_deployments(&fixture_deployments(), "testnet").unwrap();
        let btc = cat.lookup("0xpkg::tbtc::TBTC").unwrap();
        assert_eq!(btc.symbol, "TBTC");
        assert_eq!(btc.decimals, 8);
        // testTokens-only entry still present.
        let usdc = cat.lookup("0xpkg::tusdc::TUSDC").unwrap();
        assert_eq!(usdc.symbol, "TUSDC");
        assert_eq!(usdc.decimals, 6);
    }

    #[test]
    fn unknown_coin_type_returns_none() {
        let cat = TokenCatalog::from_deployments(&fixture_deployments(), "testnet").unwrap();
        assert!(cat.lookup("0xpkg::unknown::X").is_none());
    }

    /// The real-world bug we're fixing: deployments.json carries the
    /// short, `0x`-prefixed form, but chain events carry the canonical
    /// (no-prefix, zero-padded, lowercase) form. Both must hit the same
    /// catalog entry.
    #[test]
    fn lookup_matches_both_short_and_canonical_address_forms() {
        let mut tokens = BTreeMap::new();
        tokens.insert(
            "TBTC".to_string(),
            TokenInfo {
                coin_type:
                    "0x0b756179b7ae9efea2fdfb805308443bab763605459b92947616e0a04136d843::tbtc::TBTC"
                        .into(),
                faucet_id: "0x1".into(),
                decimals: 8,
            },
        );
        let deps = Deployments {
            mainnet: None,
            devnet: None,
            testnet: Some(NetworkDeployment {
                package_info: PackageInfo {
                    package_id: "0xp".into(),
                    admin_cap_id: "0xa".into(),
                    protocol_config_id: "0xc".into(),
                    upgrade_cap_id: "0xu".into(),
                    treasury_id: None,
                    publish_digest: "x".into(),
                    init_digest: None,
                    deployer: "0xd".into(),
                    deployed_at: "".into(),
                    network: "testnet".into(),
                    test_tokens: Some(shared::deployments::TestTokens {
                        package_id: "0xtp".into(),
                        upgrade_cap_id: "0xtu".into(),
                        publish_digest: "y".into(),
                        deployed_at: "".into(),
                        tokens,
                    }),
                },
                token_info: BTreeMap::new(),
            }),
        };
        let cat = TokenCatalog::from_deployments(&deps, "testnet").unwrap();

        // The form deployments.json stores.
        let a = cat
            .lookup("0x0b756179b7ae9efea2fdfb805308443bab763605459b92947616e0a04136d843::tbtc::TBTC")
            .expect("0x-prefixed lookup");
        // The canonical form chain events emit (no 0x, lowercase, padded).
        let b = cat
            .lookup("0b756179b7ae9efea2fdfb805308443bab763605459b92947616e0a04136d843::tbtc::TBTC")
            .expect("canonical-form lookup");
        assert_eq!(a.symbol, "TBTC");
        assert_eq!(a.symbol, b.symbol);
    }

    #[test]
    fn lookup_handles_short_address_in_event() {
        // Hypothetical: an event might compact-print an address. We left-pad
        // both sides, so short and full forms collide on the same key.
        let mut tokens = BTreeMap::new();
        tokens.insert(
            "SUI".to_string(),
            TokenInfo {
                coin_type: "0x2::sui::SUI".into(),
                faucet_id: "0x1".into(),
                decimals: 9,
            },
        );
        let deps = Deployments {
            mainnet: None,
            devnet: None,
            testnet: Some(NetworkDeployment {
                package_info: PackageInfo {
                    package_id: "0xp".into(),
                    admin_cap_id: "0xa".into(),
                    protocol_config_id: "0xc".into(),
                    upgrade_cap_id: "0xu".into(),
                    treasury_id: None,
                    publish_digest: "x".into(),
                    init_digest: None,
                    deployer: "0xd".into(),
                    deployed_at: "".into(),
                    network: "testnet".into(),
                    test_tokens: Some(shared::deployments::TestTokens {
                        package_id: "0xtp".into(),
                        upgrade_cap_id: "0xtu".into(),
                        publish_digest: "y".into(),
                        deployed_at: "".into(),
                        tokens,
                    }),
                },
                token_info: BTreeMap::new(),
            }),
        };
        let cat = TokenCatalog::from_deployments(&deps, "testnet").unwrap();
        let padded = format!("{:0>64}", "2");
        assert!(cat.lookup(&format!("{padded}::sui::SUI")).is_some());
    }
}
