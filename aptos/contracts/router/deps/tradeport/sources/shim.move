/// Link-time shim for Tradeport v1 (`0xe11c...3c26`).
/// Signature verified against the on-chain ABI 2026-09-03:
/// `listings::buy_token(&signer, address, String, String, u64)` (entry, no generics).
address tradeport_v2 {
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
