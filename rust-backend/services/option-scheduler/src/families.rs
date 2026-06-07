//! Pair matching against indexer bucket types.
//!
//! The scheduler decides rolls from its own DB, never from indexer-flowed
//! state. The one thing it asks the indexer is "did a bucket for this
//! (pair, expiry) actually land?" — answered by a just-in-time GraphQL query
//! (see `db::confirm` in `main.rs`). This module canonicalizes the configured
//! pair coin types so that query's results match regardless of address form.
//!
//! Type-string canonicalization
//! ----------------------------
//!
//! `BucketCreated.asset_type` is a `TypeName` BCS-decoded into a string, of
//! the form `<package-address>::<module>::<Type>`. The package address may
//! be either full-padded (`0x0b75…`) or shortened depending on the source,
//! and `TokenInfo.coin_type` in `deployments.json` may use either
//! convention. We normalise both via `ObjectID::from_str(...)` on the
//! first segment before comparing, so a configured TUSDC matches the
//! same Coin regardless of which form the chain emitted.

use std::str::FromStr;

use anyhow::{anyhow, Context, Result};

use protocol_types::asset::AssetType;
use sui_types::base_types::ObjectID;

/// Canonical `(package, module, type)` triple. Used to match the indexer's
/// type-string against configured token coin types.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CanonicalType {
    pub package: ObjectID,
    pub module: String,
    pub type_name: String,
}

impl CanonicalType {
    pub fn parse(s: &str) -> Result<Self> {
        let mut parts = s.splitn(3, "::");
        let pkg = parts
            .next()
            .ok_or_else(|| anyhow!("malformed type string {s}"))?;
        let module = parts
            .next()
            .ok_or_else(|| anyhow!("type string missing module: {s}"))?;
        let type_name = parts
            .next()
            .ok_or_else(|| anyhow!("type string missing type name: {s}"))?;
        // Allow the inner `<>` generics to remain on the type name — only
        // bare Coin types ever flow through this path today, but it costs
        // nothing to be forgiving.
        let package = ObjectID::from_hex_literal(pkg)
            .or_else(|_| ObjectID::from_str(pkg))
            .with_context(|| format!("parsing package id in type string {s}"))?;
        Ok(Self {
            package,
            module: module.to_owned(),
            type_name: type_name.to_owned(),
        })
    }
}

/// One configured pair, with both halves canonicalised so live events
/// match without re-parsing.
#[derive(Debug, Clone)]
pub struct PairKey {
    pub underlying_symbol: String,
    pub settlement_symbol: String,
    pub underlying: CanonicalType,
    pub settlement: CanonicalType,
}

impl PairKey {
    /// Does an indexed bucket's `(asset_type, settlement_type)` match this
    /// configured pair? Canonicalizes both sides so short vs full-padded
    /// package addresses compare equal.
    pub fn matches_assets(&self, asset: &AssetType, settlement: &AssetType) -> bool {
        Self::asset_matches(&self.underlying, asset.as_str())
            && Self::asset_matches(&self.settlement, settlement.as_str())
    }

    fn asset_matches(canon: &CanonicalType, raw: &str) -> bool {
        match CanonicalType::parse(raw) {
            Ok(other) => &other == canon,
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol_types::asset::AssetType;

    fn pair_key() -> PairKey {
        PairKey {
            underlying_symbol: "TBTC".into(),
            settlement_symbol: "TUSDC".into(),
            underlying: CanonicalType::parse(
                "0x0b756179b7ae9efea2fdfb805308443bab763605459b92947616e0a04136d843::tbtc::TBTC",
            )
            .unwrap(),
            settlement: CanonicalType::parse(
                "0x0b756179b7ae9efea2fdfb805308443bab763605459b92947616e0a04136d843::tusdc::TUSDC",
            )
            .unwrap(),
        }
    }

    #[test]
    fn matches_short_package_form() {
        // ObjectID::from_hex_literal pads leading zeros up to 32 bytes, so
        // a chain that emits "0x2::sui::SUI" parses to the same canonical
        // type as "0x000…02::sui::SUI". This is the form everyone trips on
        // first; the test just locks the round-trip.
        let short = CanonicalType::parse("0x2::sui::SUI").unwrap();
        let long = CanonicalType::parse(
            "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI",
        )
        .unwrap();
        assert_eq!(short, long);
    }

    #[test]
    fn matches_configured_pair_assets() {
        let p = pair_key();
        let asset = AssetType::new(
            "0x0b756179b7ae9efea2fdfb805308443bab763605459b92947616e0a04136d843::tbtc::TBTC",
        );
        let settlement = AssetType::new(
            "0x0b756179b7ae9efea2fdfb805308443bab763605459b92947616e0a04136d843::tusdc::TUSDC",
        );
        assert!(p.matches_assets(&asset, &settlement));
    }

    #[test]
    fn rejects_unknown_pair_assets() {
        let p = pair_key();
        let asset = AssetType::new("0x2::sui::SUI");
        let settlement = AssetType::new(
            "0x0b756179b7ae9efea2fdfb805308443bab763605459b92947616e0a04136d843::tusdc::TUSDC",
        );
        assert!(!p.matches_assets(&asset, &settlement));
    }
}
