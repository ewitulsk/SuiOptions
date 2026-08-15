//! Coin-type → {symbol, decimals} lookup.
//!
//! Built once at startup from the supported-token catalog fetched from the
//! token-info service. Keyed by the fully-qualified Move type — that's what
//! every chain event carries in its `AssetType` field, so handlers can
//! resolve a bucket's `asset_type` / `settlement_type` to display data in
//! one hash lookup.

use std::collections::HashMap;

use tracing::{debug, info};

use token_info_client::SupportedToken;

#[derive(Clone, Debug)]
pub struct TokenMeta {
    pub symbol: String,
    pub decimals: u8,
    /// Pyth feed id, when token-info carries one. The `/buckets` ladder needs
    /// it to ask oracle-service for realized vol, which is keyed by Pyth feed
    /// even on deployments whose *prices* come from Switchboard (SO-346) —
    /// the vol path resolves through Pyth Benchmarks either way.
    pub pyth_feed_id: Option<String>,
}

#[derive(Default, Debug)]
pub struct TokenCatalog {
    by_coin_type: HashMap<String, TokenMeta>,
}

impl TokenCatalog {
    /// Build the catalog from the token-info supported-token list. The
    /// `ticker` becomes the display symbol; `decimals` is taken verbatim.
    pub fn from_tokens(tokens: &[SupportedToken]) -> Self {
        let mut by_coin_type: HashMap<String, TokenMeta> = HashMap::new();
        for t in tokens {
            by_coin_type.insert(
                normalize_coin_type(&t.coin_type),
                TokenMeta {
                    symbol: t.ticker.clone(),
                    decimals: t.decimals,
                    pyth_feed_id: t.pyth_feed_id.clone(),
                },
            );
        }
        info!(
            tokens = by_coin_type.len(),
            "built token catalog from token-info"
        );
        for (coin_type, meta) in &by_coin_type {
            debug!(%coin_type, symbol = %meta.symbol, decimals = meta.decimals, "catalog entry");
        }
        Self { by_coin_type }
    }

    /// Look up by the Move type string carried in chain events. Returns
    /// `None` if the catalog has no entry — handlers should fall back to
    /// the raw string in that case rather than dropping the bucket.
    pub fn lookup(&self, coin_type: &str) -> Option<&TokenMeta> {
        self.by_coin_type.get(&normalize_coin_type(coin_type))
    }

    /// Reverse lookup for the spec endpoint: catalog symbol → chain-form
    /// coin type. Case-insensitive on the ticker.
    pub fn by_symbol(&self, symbol: &str) -> Option<&str> {
        self.by_coin_type
            .iter()
            .find(|(_, m)| m.symbol.eq_ignore_ascii_case(symbol))
            .map(|(k, _)| k.as_str())
    }
}

/// Normalize a Move type string so address-format differences don't break
/// lookups. Chain events carry the canonical form (no `0x` prefix,
/// 64-hex-char address); token-info stores the convenient form (`0x`-prefixed,
/// possibly trimmed). We canonicalize to the chain form: strip leading `0x`,
/// lowercase, left-pad the address to 64 hex chars.
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

    fn tok(ticker: &str, coin_type: &str, decimals: u8) -> SupportedToken {
        SupportedToken {
            coin_type: coin_type.into(),
            ticker: ticker.into(),
            name: ticker.into(),
            logo_uri: None,
            decimals,
            pyth_feed_id: None,
            switchboard_feed_id: None,
            enabled: true,
        }
    }

    #[test]
    fn builds_from_tokens() {
        let cat = TokenCatalog::from_tokens(&[
            tok("TBTC", "0xpkg::tbtc::TBTC", 8),
            tok("TUSDC", "0xpkg::tusdc::TUSDC", 6),
        ]);
        let btc = cat.lookup("0xpkg::tbtc::TBTC").unwrap();
        assert_eq!(btc.symbol, "TBTC");
        assert_eq!(btc.decimals, 8);
        let usdc = cat.lookup("0xpkg::tusdc::TUSDC").unwrap();
        assert_eq!(usdc.symbol, "TUSDC");
        assert_eq!(usdc.decimals, 6);
    }

    #[test]
    fn unknown_coin_type_returns_none() {
        let cat = TokenCatalog::from_tokens(&[tok("TBTC", "0xpkg::tbtc::TBTC", 8)]);
        assert!(cat.lookup("0xpkg::unknown::X").is_none());
    }

    /// token-info carries the short, `0x`-prefixed form, but chain events
    /// carry the canonical (no-prefix, zero-padded, lowercase) form. Both
    /// must hit the same catalog entry.
    #[test]
    fn lookup_matches_both_short_and_canonical_address_forms() {
        let cat = TokenCatalog::from_tokens(&[tok(
            "TBTC",
            "0x0b756179b7ae9efea2fdfb805308443bab763605459b92947616e0a04136d843::tbtc::TBTC",
            8,
        )]);
        let a = cat
            .lookup(
                "0x0b756179b7ae9efea2fdfb805308443bab763605459b92947616e0a04136d843::tbtc::TBTC",
            )
            .expect("0x-prefixed lookup");
        let b = cat
            .lookup("0b756179b7ae9efea2fdfb805308443bab763605459b92947616e0a04136d843::tbtc::TBTC")
            .expect("canonical-form lookup");
        assert_eq!(a.symbol, "TBTC");
        assert_eq!(a.symbol, b.symbol);
    }

    #[test]
    fn lookup_handles_short_address_in_event() {
        let cat = TokenCatalog::from_tokens(&[tok("SUI", "0x2::sui::SUI", 9)]);
        let padded = format!("{:0>64}", "2");
        assert!(cat.lookup(&format!("{padded}::sui::SUI")).is_some());
    }
}
