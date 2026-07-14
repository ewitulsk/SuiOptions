//! Pair matching against indexer bucket/vault mints.
//!
//! On Sui this module canonicalized Move type strings (short vs full-padded
//! package addresses). On Solana ids are base58 SPL mint addresses compared
//! byte-exact — no normalization — so [`PairKey`] collapses to the symbols
//! plus the two mint strings.

/// One configured pair: symbols (the DB key) plus the resolved mints (the
/// chain key the indexer's views report).
#[derive(Debug, Clone)]
pub struct PairKey {
    pub underlying_symbol: String,
    pub settlement_symbol: String,
    pub underlying_mint: String,
    pub settlement_mint: String,
}

impl PairKey {
    /// Does an indexed bucket/vault's `(underlying_mint, settlement_mint)`
    /// match this configured pair? Byte-exact — base58 needs no
    /// normalization.
    pub fn matches_mints(&self, underlying_mint: &str, settlement_mint: &str) -> bool {
        self.underlying_mint == underlying_mint && self.settlement_mint == settlement_mint
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> PairKey {
        PairKey {
            underlying_symbol: "TBTC".into(),
            settlement_symbol: "TUSDC".into(),
            underlying_mint: "So11111111111111111111111111111111111111112".into(),
            settlement_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
        }
    }

    #[test]
    fn matches_exact_mints() {
        let p = pair();
        assert!(p.matches_mints(
            "So11111111111111111111111111111111111111112",
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
        ));
    }

    #[test]
    fn rejects_swapped_or_near_miss_mints() {
        let p = pair();
        // Swapped legs.
        assert!(!p.matches_mints(
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "So11111111111111111111111111111111111111112"
        ));
        // Case-different base58 is a different address, not a form of the
        // same one.
        assert!(!p.matches_mints(
            "so11111111111111111111111111111111111111112",
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
        ));
    }
}
