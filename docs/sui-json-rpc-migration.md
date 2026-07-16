# Sui JSON-RPC → gRPC/GraphQL migration

Sui deprecated the fullnode JSON-RPC API and scheduled its deactivation for
**July 2026** (<https://docs.sui.io/develop/accessing-data/json-rpc-migration>).
This is not a future problem for us:

> **As of 2026-07-15, `https://fullnode.testnet.sui.io:443` returns HTTP 404
> for every JSON-RPC method.** Mainnet and devnet public fullnodes still
> answer, but testnet — the network our staging *and* prod run against — is
> already dark. Anything still speaking JSON-RPC to the public testnet
> fullnode is broken today; the Rust services only keep working where the
> `[sui] rpc_url` secrets override points at a third-party provider that
> still serves JSON-RPC (see `secrets.example.toml`). That is a stay of
> execution, not a fix.

The replacements:

| API | Use for | Verified endpoints |
|---|---|---|
| **gRPC** (`sui.rpc.v2.*`) | backend reads, simulation, tx execution, checkpoint streaming | `https://fullnode.{testnet,mainnet,devnet}.sui.io:443` — also serves gRPC-web with CORS `*`, so browsers can call it directly |
| **GraphQL RPC** | filterable historical queries (events, tx lookups), frontend reads | `https://graphql.{testnet,mainnet,devnet}.sui.io/graphql` (CORS `*`) |

Both endpoints and every query/call named below were verified live against
testnet on 2026-07-15.

## JSON rendering differences (critical when porting reads)

The gRPC `json` object rendering and GraphQL `contents { json }` are
identical to each other but **not** to JSON-RPC's `content.fields`:

| Move value | JSON-RPC `content.fields` | gRPC/GraphQL JSON |
|---|---|---|
| `u64`/`u128` | decimal string | decimal string (same) |
| nested struct | wrapped: `{"type": …, "fields": {…}}` | fields nested **directly**, no wrapper |
| enum (e.g. `vault::Phase`) | `{"variant": "Active", "fields": {}}` | `{"@variant": "Active"}` — note the `@` |
| `Balance<T>` / `Supply<T>` | bare value | `{"value": "…"}` |
| `Option<T>` | null / inner | null / inner (same) |
| `vector<u8>` | number array | **base64 string** |
| `UID` (`id` field) | `{"id": "0x…"}` | bare `"0x…"` |
| coin types in balance APIs | short form (`0x2::sui::SUI`) | fully padded (`0x000…002::sui::SUI`) — always compare via `normalizeStructTag` / `to_canonical()` (see `move-type-normalization.md`) |

Pagination cursors are **not portable** from JSON-RPC — never persist and
replay an old cursor against the new APIs.

## Inventory and status

### ✅ Migrated (this change)

**Frontend (`frontend/`)** — all chain traffic now goes through
`src/lib/suiGrpc.ts` (per-network `SuiGrpcClient` from `@mysten/sui/grpc`,
plus a small GraphQL fetch helper):

| Call site | Was (JSON-RPC) | Now |
|---|---|---|
| `api/useCoinBalance.ts` | `getBalance` | gRPC `StateService.GetBalance` (`client.core.getBalance`) |
| `api/useVaults.ts` (`useShareBalance`) | `getBalance` | same as above |
| `api/useOwnedCallOptions.ts`, `api/useOwnedPutOptions.ts` | `getAllBalances` | gRPC `StateService.ListBalances`, paged (`listAllBalances`) |
| `api/useOwnedPositions.ts`, `api/useAdminCap.ts`, `api/useVaults.ts` (`useOwnedVaultReceipts`) | `getOwnedObjects` + `showContent` | gRPC `StateService.ListOwnedObjects` with `type` filter + `json` include (`listAllOwnedObjects`) |
| `api/deepbook.ts` (`devInspect` helper → order book, open orders, BM balances) | `devInspectTransactionBlock` | gRPC `TransactionExecutionService.SimulateTransaction` with `checksEnabled: false` + `commandResults` (BCS return values, same decode path) |
| `api/deepbook.ts` (`findBalanceManager`) | `queryEvents` descending | GraphQL `events(filter: {type}, last: 50, before: cursor)` walked newest-first |
| `tx/submit.ts` | wallet `signAndExecuteTransaction` + `executeTransactionBlock` | tx built via gRPC (resolution + gas selection), wallet signs the exact bytes, gRPC `ExecuteTransaction` submits — both sponsored and wallet-paid paths |

dapp-kit note: `@mysten/dapp-kit` (1.x, incl. latest 1.1.5) still hard-wires
`SuiJsonRpcClient` into its provider, so `main.tsx` keeps the JSON-RPC
network map **for wallet plumbing only** — no reads or writes route through
it anymore. When a stable dapp-kit ships on the new client interface
(currently only under the npm `experimental` dist-tag), swap the provider and
delete the map.

**api-service (`rust-backend/services/api-service/`)** — the only Rust
service that was hand-rolling JSON-RPC over reqwest:

- `src/sui_rpc.rs`: `sui_getObject` → GraphQL
  `object(address:) { asMoveObject { contents { json } } }`, parser adapted
  to the new rendering (`@variant`, unnested `config`). Golden tests updated
  against a live testnet vault.
- Config key renamed `sui_rpc_url` → `sui_graphql_url`
  (default `https://graphql.testnet.sui.io/graphql`); updated in
  `config/config.{staging,prod}.toml`.

**CI (`.github/workflows/redeploy-contract.yml`)** — the publish-checkpoint
lookup: raw `curl` `sui_getTransactionBlock` → GraphQL
`transaction(digest:) { effects { checkpoint { sequenceNumber } } }`.

### ⏳ Remaining (phase 3): `sui-tx` and the Rust services

Everything below still uses the JSON-RPC `sui_sdk::SuiClient` (pinned git
`framework/mainnet`) and **only works while the third-party `rpc_url`
override holds**:

| Surface | JSON-RPC usage | Replacement |
|---|---|---|
| `crates/sui-tx` (`tx/*.rs`) — the wrapper every service funnels through | `read_api().get_object_with_options`, `coin_read_api()`, `get_reference_gas_price`, `dev_inspect_transaction_block`, `dry_run_transaction_block`, `quorum_driver_api().execute_transaction_block`, `event_api().query_events` | gRPC `LedgerService.GetObject` / `StateService.ListOwnedObjects` / `LedgerService.GetEpoch` (RGP) / `TransactionExecutionService.SimulateTransaction` / `….ExecuteTransaction`; events via GraphQL or checkpoint ingestion |
| `services/price-charting` (`watcher.rs`) | `query_events` poll loop | GraphQL `events` poll, or reuse the indexer's checkpoint stream |
| `services/balance-monitor` | `coin_read_api().get_balance` | gRPC `StateService.GetBalance` |
| `services/cctp-relay` | `get_transaction_with_options` | gRPC `LedgerService.GetTransaction` |
| `services/keeper` | `get_dynamic_field_object` (only user), `get_object_with_options`, `query_events`, plus a **hardcoded** `https://fullnode.testnet.sui.io:443` in `discovery.rs` | derive the dynamic-field id locally + `GetObject`; kill the hardcoded URL when touched |
| `services/option-scheduler`, `services/mm-bot`, `services/gas-station` | via `SuiClientWrapper` + assorted reads | falls out of the `sui-tx` migration |
| `services/indexer` | already checkpoint-ingestion (`sui-data-ingestion-core` — **not** affected by the deactivation); only the boot/tip poll uses `get_latest_checkpoint_sequence_number` | gRPC `LedgerService.GetServiceInfo` (returns checkpoint height) or GraphQL `checkpoint { sequenceNumber }` |
| `tools/deployment-manager`, `tools/{trader,writer,exchange}`, `tools/deepbook-pool-test`, `crates/move-publish` | publish/execute via `quorum_driver_api`, misc reads | gRPC `ExecuteTransaction` + reads as above |
| `session-tokens/demo-frontend` | `SuiJsonRpcClient`: `getBalance`, `getCoins`, `signAndExecuteTransaction` | apply the same recipe as `frontend/` (`suiGrpc.ts` pattern); low priority — demo only |

**Recommended path for the Rust backend:** the pinned sui monorepo rev
already contains the `sui-rpc-api`/`sui-rpc` crates (they're in
`Cargo.lock` as transitive deps), so the gRPC client can be added *at the
same pin* — no version bump required:

```toml
# rust-backend/Cargo.toml [workspace.dependencies]
sui-rpc-api = { git = "https://github.com/mystenlabs/sui", package = "sui-rpc-api", branch = "framework/mainnet" }
```

Migrate `SuiClientWrapper` internals method-by-method (it's the choke point —
services keep their call sites), starting with reads, then
simulate/dry-run, then execution. `sui-types` stays; the gRPC proto types
convert to/from `sui-types` at the same rev. Alternatively adopt the
standalone [sui-rust-sdk](https://github.com/MystenLabs/sui-rust-sdk)
(`sui-transaction-builder` + gRPC client), but that swaps the whole type
system and is a much bigger diff.

**Interim mitigation (until phase 3 lands):** keep `[sui] rpc_url` in every
deployed service's secrets pointed at a provider that still serves testnet
JSON-RPC, and treat any `tx-failed-…` / RPC-connect alert from a service as
a possible provider shutdown.

## Gotchas carried over from the official guide

- **No archival fallback**: new-API fullnodes prune (testnet advertised
  `lowest-available-checkpoint` ≈ 358M at time of writing). Old history
  needs the Archival Service — only relevant to us for deep event backfills.
- **WebSocket subscriptions are gone** — we never used them (indexer is
  checkpoint-based, everything else polls), so nothing to do.
- **Public endpoints are best-effort**: for production-grade latency Mysten
  recommends a private fullnode or a dedicated provider; our gRPC/GraphQL
  URLs are config/constants and can be repointed the same way `rpc_url` was.
- **`unsafe_*` builder methods** were never used here (all PTBs are built
  with the SDKs).
