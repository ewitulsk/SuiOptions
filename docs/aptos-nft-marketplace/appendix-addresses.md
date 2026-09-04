# Appendix: Aptos NFT marketplace contract addresses (mainnet)

Read from mainnet on 2026-09-02. "Upgrade" is the `0x1::code::PackageRegistry`
policy: 1 = compatible (public function signatures cannot break), 2 =
immutable.

| Venue / package | Account | Upgrade | Upgrades so far | Source published |
|---|---|---|---|---|
| Tradeport `tradeport` (v1: `listings`, `biddings`, `markets`, `fees`, `transfers`) | `0xe11c12ec495f3989c35e1c6a0af414451223305b579291fc8f3d9d0575a23c26` | 1 | 18 | no |
| Tradeport `tradeport_v2` (`listings_v2`, `biddings_v2`, `markets_v2`, `fees_v2`, `transfers_v2`) | same | 1 | 20 | no |
| Tradeport adapters `wapal`, `topaz_v2`, `bluemove_v2` (one module each) | same | 1 | 0-1 | no |
| Tradeport `tradeport_loyalty`, `launchpad`, `tradeport_megamint` | same | 1 | | no |
| Wapal `Marketplace` | `0x584b50b999c78ade62f8359c91b5165ff390338d45f8e55969a04e65d76258c9` | 1 | 15 | no |
| Rarible `rarible-marketplace` | `0x465a0051e8535859d4794f0af24dbf35c5349bedadab26404b20b825035ee790` | 2 | 0 | yes |
| Bluemove `MarketPlace` (v1 tokens) | `0xd1fd99c1944b84d1670a2536417e997864ad12303d19eac725891691b04d614e` | 1 | 21 | yes |
| Bluemove `bluemove_marketplace_token_v2` | `0xd520d8669b0a3de23119898dcdff3e0a27910db247663646ad18cf16e44c6f5` | 1 | | no |
| OKX `okx-nft-marketplace` | `0x1e6009ce9d288f3d5031c06ca0b19a334214ead798a0cb38808485bd6d997a43` | 1 | | no |
| Topaz `Topaz` (v1) | `0x2c7bccf7b31baf770fdbcc768d9e9cb3d87805e255355df5db32ac9a669010a2` | 1 | | no |
| Topaz v2 `Marketplace` | `0x6de37368e31dff4580b211295198159ee6f98b42ffa93c5683bb955ca1be67e0` | 1 | | no |
| Souffl3 `souffl3` (`Aggregator`, `FixedPriceMarket`) | `0xf6994988bd40261af9431cd6dd3fcf765569719e66322c7a05cc78a89cd366d4` | | | |
| Seashrine `seashrine_market` | `0xd5431191333a6185105c172e65f9fcd945ae92159ab648e1a9ea88c71e275548` | | | |
| Ozozoz `OzozozMarketplace` | `0xded0c1249b522cecb11276d2fad03e6635507438fef042abeea3097846090bcd` | | | |
| `hybrid` (dependency of Wapal and Tradeport v2; hybrid FA/NFT tokens) | `0xbbe8a08f3b9774fccb31e02def5a79f1b7270b2a1cb9ffdc05b2622813298f2a` | | | |
| `OnlyOnAptos` (Tradeport megamint launchpad dependency) | `0x39673a89d85549ad0d7bef3f53510fe70be2d5abaac0d079330ade5548319b62` | | | |

Framework addresses: `0x1` (AptosFramework, `aptos_coin`, `object`,
`fungible_asset`), `0x3` (legacy `token`), `0x4` (`token`, `collection`,
`royalty`, `aptos_token`).

Endpoints used:

- Fullnode REST: `https://fullnode.mainnet.aptoslabs.com/v1`
  (`/accounts/{addr}/modules`, `/accounts/{addr}/resource/0x1::code::PackageRegistry`)
- Indexer GraphQL: `https://api.mainnet.aptoslabs.com/v1/graphql`
- Transaction Stream: `grpc.mainnet.aptoslabs.com:443` (API key required)
- Docs for each venue's event mapping:
  `https://aptos.dev/build/indexer/nft-aggregator/marketplaces/{tradeport,wapal,bluemove,rarible,topaz}`
- Aggregator processor source: `https://github.com/aptos-labs/aptos-nft-aggregator`
- Reference marketplace: `https://github.com/aptos-labs/aptos-core/tree/main/aptos-move/move-examples/marketplace`
