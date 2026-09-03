/// Link-time shim for Bluemove v2 (`0xd520...6f5`).
/// Signatures verified against the on-chain ABI 2026-09-03:
/// `coin_listing::price<CoinType>(Object<Listing>) -> Option<u64>` (view),
/// `coin_listing::purchase<CoinType>(&signer, Object<Listing>, u64)` (entry).
/// Never published; on mainnet these addresses resolve to Bluemove's code.
address bluemove_v2 {
module listing {
    struct Listing has key {}
}

module coin_listing {
    use std::option::Option;
    use aptos_framework::object::Object;
    use bluemove_v2::listing::Listing;

    #[view]
    public fun price<CoinType>(listing: Object<Listing>): Option<u64> {
        abort 0
    }

    public entry fun purchase<CoinType>(
        buyer: &signer,
        listing: Object<Listing>,
        price: u64,
    ) {
        abort 0
    }
}
}
