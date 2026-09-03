/// Link-time shim for Tradeport v2 (`0xe11c...3c26`).
/// Signature verified against the on-chain ABI 2026-09-03:
/// `listings_v2::buy_token(&signer, Object<Listing>)` (entry, no generics).
address tradeport_v2 {
module listings_v2 {
    use aptos_framework::object::Object;

    struct Listing has key {}

    public entry fun buy_token(buyer: &signer, listing: Object<Listing>) {
        abort 0
    }
}
}
