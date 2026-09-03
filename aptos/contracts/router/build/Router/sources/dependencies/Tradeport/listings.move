/// Link-time shim for Tradeport (`0xe11c...3c26`).
/// Signatures verified against the on-chain ABI 2026-09-03:
/// `listings_v2::buy_token(&signer, Object<Listing>)` (entry, no generics),
/// `listings::buy_token(&signer, address, String, String, u64)` (entry, no generics).
/// Never published; on mainnet these addresses resolve to Tradeport's code.
address tradeport_v2 {
module listings_v2 {
    use aptos_framework::object::Object;

    struct Listing has key {}

    public entry fun buy_token(buyer: &signer, listing: Object<Listing>) {
        abort 0
    }
}

module listings {
    use std::string::String;

    public entry fun buy_token(
        buyer: &signer,
        creator: address,
        collection: String,
        name: String,
        property_version: u64,
    ) {
        abort 0
    }
}
}
