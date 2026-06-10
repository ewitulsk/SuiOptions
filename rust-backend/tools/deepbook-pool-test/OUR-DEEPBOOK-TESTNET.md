# Our self-hosted DeepBook v3 — Sui testnet

Deployed 2026-06-10 to dodge the 500-DEEP pool-creation fee. We published our
**own** copy of the DeepBook `token` (DEEP) and `deepbook` packages from source,
so we control DEEP supply (10B minted to the deployer at init) and can create
permissionless pools freely. Contract logic is the upstream v8 source unmodified;
the only change is `deepbook` links against *our* DEEP instead of Mysten's.

- Source / Move.toml + Published.toml edits live in `/root/Dev/hackathon/deepbookv3-deploy`
  (clone of MystenLabs/deepbookv3 @ `e276939`, source `current_version()` = 8).
- v8 vs the deployed-testnet v5 we profiled: `create_permissionless_pool`,
  `place_limit_order`, and the `OrderFilled` event are byte-for-byte identical,
  so the integration surface is unchanged.
- Chain: testnet (`4c78adac`). Deployer/admin/treasury:
  `0xab8d1b5a5311c9400e3eaf5c3b641f10fb48b43cc30d365fa8a98a6ca6bd4865`.

## token package (our DEEP)

| Thing | Value |
|---|---|
| Package id | `0x839472bcaa05bda0b7b26c69d7acba462829047bdbdb67a00a1d3b876e72220e` |
| DEEP type | `0x839472bcaa05bda0b7b26c69d7acba462829047bdbdb67a00a1d3b876e72220e::deep::DEEP` |
| ProtectedTreasury (shared) | `0xad03716fbb0a6adb5605b08446babe89a99aa333b05d397f9983c43166ae0718` |
| CoinMetadata | `0x6c35baf7f340df8ca9de3a631ccda2c1ca59e9d157da285de389cfba48f2042d` |
| UpgradeCap (owned) | `0x8054013a099d6cab875af728f16952b9b6befd4a7c8900133c9b22b11ad6e72f` |
| 10B DEEP coin (owned) | `0x9024e99ff32f153bf0c0da00202827542925c08fdbbbf7eca2d268a50ca3ae03` |
| publish digest | `HschNoUWRd6A9QXCqWJVgvoxuubC2i82pmU42nQxLfLs` |

## deepbook package (our order book)

| Thing | Value |
|---|---|
| Package id | `0x725378acc0bb8b9273cef82171f64f87d5c0348b189318d3f682db5024f42431` |
| Registry (shared) | `0x13ab9d66e0558ad91f98031d9cd6dcbf4c6318f834b5427f253d8261947f9156` |
| DeepbookAdminCap (owned) | `0xd3840fca3ece5be486d6510201adff58c4b6cdf2bd680c780f8f8382446beb01` |
| UpgradeCap (owned) | `0xdf8c9aaf6a789958824957541fc007cdd9a355702a11534e1fdef39d7139340e` |
| publish digest | `HTHo69RVV1MeVo6TCy1vKgYcHZ8mUws867FuyYwgVaU5` |

## Pool<TBTC, TUSDC> (proof — created with our DEEP)

| Thing | Value |
|---|---|
| Pool id | `0xa2914ce8325f82b56888bdbb09183ef02394d62f9a5bc9f812803fae1c53b7f0` |
| tick / lot / min | `10000 / 1000 / 10000` |
| taker / maker fee | `0.1%` / `0.05%` |
| create digest | `EDR2ryuZooBkvnw36aCqatbuWNcEherpdaiwCU6TZY6f` |

## To point `deepbook-pool-test` (or any integration) at this deployment

Swap these constants in `src/main.rs`:
- `DEEPBOOK_PKG` → `0x725378acc0bb8b9273cef82171f64f87d5c0348b189318d3f682db5024f42431`
- `REGISTRY_ID` → `0x13ab9d66e0558ad91f98031d9cd6dcbf4c6318f834b5427f253d8261947f9156`
- `DEEP_TYPE`  → `0x839472bcaa05bda0b7b26c69d7acba462829047bdbdb67a00a1d3b876e72220e::deep::DEEP`

The `acquire_deep` / `DEEP_SUI_POOL` swap path is now dead — we already hold 10B DEEP.

> Watchers (SO-156): all `OrderFilled` / `PoolCreated` type strings now resolve to
> package `0x725378…`, **not** `0xfb28c4…`. Parameterize watcher configs by package id.
