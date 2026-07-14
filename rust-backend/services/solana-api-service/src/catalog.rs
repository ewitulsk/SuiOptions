//! Mint → {symbol, decimals} lookup.
//!
//! Built once at startup from the supported-token catalog fetched from the
//! solana-token-info service. Keyed by the SPL mint address (base58) —
//! that's what every chain event and indexer view carries in its
//! `*_mint` fields. Byte-exact comparison: base58 needs no normalization
//! (unlike the Sui twin's Move-type canonicalization).

use std::collections::HashMap;

use tracing::{debug, info};

use solana_token_info_client::SupportedToken;

#[derive(Clone, Debug)]
pub struct TokenMeta {
    pub symbol: String,
    pub decimals: u8,
}

#[derive(Default, Debug)]
pub struct TokenCatalog {
    by_mint: HashMap<String, TokenMeta>,
}

impl TokenCatalog {
    /// Build the catalog from the solana-token-info supported-token list.
    /// The `ticker` becomes the display symbol; `decimals` is taken
    /// verbatim.
    pub fn from_tokens(tokens: &[SupportedToken]) -> Self {
        let mut by_mint: HashMap<String, TokenMeta> = HashMap::new();
        for t in tokens {
            by_mint.insert(
                t.mint.clone(),
                TokenMeta {
                    symbol: t.ticker.clone(),
                    decimals: t.decimals,
                },
            );
        }
        info!(
            tokens = by_mint.len(),
            "built token catalog from solana-token-info"
        );
        for (mint, meta) in &by_mint {
            debug!(%mint, symbol = %meta.symbol, decimals = meta.decimals, "catalog entry");
        }
        Self { by_mint }
    }

    /// Look up by the mint address carried in chain events / indexer views.
    /// Returns `None` if the catalog has no entry — handlers should fall
    /// back to the raw mint string rather than dropping the row.
    pub fn lookup(&self, mint: &str) -> Option<&TokenMeta> {
        self.by_mint.get(mint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(ticker: &str, mint: &str, decimals: u8) -> SupportedToken {
        SupportedToken {
            mint: mint.into(),
            ticker: ticker.into(),
            name: ticker.into(),
            logo_uri: None,
            decimals,
            pyth_feed_id: None,
            enabled: true,
        }
    }

    #[test]
    fn builds_from_tokens() {
        let cat = TokenCatalog::from_tokens(&[
            tok("TBTC", "So11111111111111111111111111111111111111112", 8),
            tok("TUSDC", "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", 6),
        ]);
        let btc = cat
            .lookup("So11111111111111111111111111111111111111112")
            .unwrap();
        assert_eq!(btc.symbol, "TBTC");
        assert_eq!(btc.decimals, 8);
        let usdc = cat
            .lookup("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v")
            .unwrap();
        assert_eq!(usdc.symbol, "TUSDC");
        assert_eq!(usdc.decimals, 6);
    }

    #[test]
    fn lookup_is_byte_exact_no_normalization() {
        let cat =
            TokenCatalog::from_tokens(&[tok("TBTC", "So11111111111111111111111111111111111111112", 8)]);
        assert!(cat.lookup("unknown").is_none());
        // Base58 is case-sensitive; a near-miss must not match.
        assert!(cat
            .lookup("so11111111111111111111111111111111111111112")
            .is_none());
    }
}
