/// Link-time shim for OKX (`0x1e60...7a43`).
/// Signature verified against the on-chain ABI 2026-09-03:
/// `okx_fixed_price::buy_direct_listing<CoinType>(&signer, address, u64)` (entry).
/// Never published; on mainnet this address resolves to OKX's code.
address okx {
module okx_fixed_price {
    public entry fun buy_direct_listing<CoinType>(
        buyer: &signer,
        listing: address,
        price: u64,
    ) {
        abort 0
    }
}
}
