//! Beta → stable feed-id mapping for the Benchmarks API.
//!
//! Sui testnet on-chain `PriceInfoObject`s — and therefore the Hermes-beta
//! *latest* prices and the on-chain feed ids the services discover — are keyed
//! by the BETA feed set. The Benchmarks API, however, only serves the STABLE
//! feed set, so a historical realized-vol lookup for a beta id 404s and the
//! caller falls back to a static sigma.
//!
//! Beta and stable are the *same* underlying symbol (e.g. `Crypto.SUI/USD`)
//! published on two Pyth networks. Mapping beta → stable here lets the
//! Benchmarks call run against the stable id and return the real asset's price
//! history instead of the static fallback — strictly better, since the test
//! tokens are meant to track those real assets.
//!
//! Pairs were matched by symbol against the live Pyth feed catalogs
//! (`hermes-beta` and stable `/v2/price_feeds`). An id with no mapping is
//! returned unchanged, so a newly added token without an entry simply degrades
//! to the prior fallback behaviour — no regression, no panic.

use crate::types::PriceFeedId;

/// `(beta, stable)` 64-hex feed ids (no `0x` prefix), matched by Pyth symbol.
const BETA_TO_STABLE: &[(&str, &str)] = &[
    // Crypto.BTC/USD
    (
        "f9c0172ba10dfa4d19088d94f5bf61d3b54d5bd7483a322a982e1373ee8ea31b",
        "e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43",
    ),
    // Crypto.SUI/USD
    (
        "50c67b3fd225db8912a424dd4baed60ffdde625ed2feaaf283724f9608fea266",
        "23d7315113f5b1d3ba7a83604c44b94d79f4fd69af77f804fc7f920a6dc65744",
    ),
    // Crypto.USDC/USD
    (
        "41f3625971ca2ed2263e78573fe5ce23e13d2558ed3f2e47ab0f84fb9e7ae722",
        "eaa020c61cc479712813461ce153894a96a6c00b21ed0cfc2798d1f9a9e9c94a",
    ),
    // Crypto.DEEP/USD
    (
        "e18bf5fa857d5ca8af1f6a458b26e853ecdc78fc2f3dc17f4821374ad94d8327",
        "29bdd5248234e33bd93d3b81100b5fa32eaa5997843847e2c2cb16d7c6d9f7ff",
    ),
    // Crypto.WAL/USD
    (
        "a6ba0195b5364be116059e401fb71484ed3400d4d9bfbdf46bd11eab4f9b7cea",
        "eba0732395fae9dec4bae12e52760b35fc1c5671e2da8b449c9af4efe5d54341",
    ),
];

/// Map a (beta) feed id to its stable equivalent for the Benchmarks API.
/// Returns `feed` unchanged when there is no known mapping.
pub fn benchmark_feed_id(feed: PriceFeedId) -> PriceFeedId {
    let hex = feed.to_hex();
    for (beta, stable) in BETA_TO_STABLE {
        if hex.eq_ignore_ascii_case(beta) {
            // Table entries are compile-time constants exercised by the tests
            // below, so this parse cannot fail in practice.
            return PriceFeedId::from_hex(stable).expect("benchmark map: valid stable feed id");
        }
    }
    feed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_every_beta_to_a_valid_distinct_stable() {
        for (beta, stable) in BETA_TO_STABLE {
            let b = PriceFeedId::from_hex(beta).expect("valid beta id");
            let s = PriceFeedId::from_hex(stable).expect("valid stable id");
            assert_eq!(benchmark_feed_id(b), s, "beta {beta} should map to {stable}");
            assert_ne!(b, s, "beta and stable ids must differ");
        }
    }

    #[test]
    fn unknown_id_passes_through_unchanged() {
        let unknown =
            PriceFeedId::from_hex("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        assert_eq!(benchmark_feed_id(unknown), unknown);
    }
}
