# Sui JSON-RPC → gRPC/GraphQL migration

Sui deprecated the fullnode JSON-RPC API and scheduled its deactivation for
**July 2026** (<https://docs.sui.io/develop/accessing-data/json-rpc-migration>).
This is not a future problem for us:

> **As of 2026-07-15, `https://fullnode.testnet.sui.io:443` returns HTTP 404
> for every JSON-RPC method.** Mainnet and devnet public fullnodes still
> answer, but testnet — the network our staging *and* prod run against — is
> already dark. Anything still speaking JSON-RPC to the public testnet
> fullnode is broken today.
>
> **Resolved as of SO-336**: the whole workspace speaks gRPC/GraphQL and no
> longer depends on a third-party JSON-RPC provider. The sections below are
> kept as the porting reference — the rendering and pagination differences
> still bite anyone touching a chain read.

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

### ✅ Migrated (phase 1–2, PR #283): frontend, api-service, CI checkpoint

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

### ✅ Migrated (phase 3, SO-336): `sui-tx` and the Rust backend

JSON-RPC is now **completely gone from the workspace** — no `sui-sdk`
client, no `read_api()`/`coin_read_api()`/`event_api()`/`quorum_driver_api()`,
no `dev_inspect_transaction_block`/`dry_run_transaction_block` anywhere. The
`sui-sdk` dependency was dropped from every crate that only used it for the
JSON-RPC client.

**The seam** — two new modules in `crates/sui-tx`:

- `chain.rs`: `ChainClient`, a gRPC client over `sui_rpc_api::client::Client`.
  Objects (`get_object`, `get_object_json`, `try_get_*`, `multi_get_objects`),
  object args (`shared_object_arg`, `owned_object_arg`, `object_arg`), coins
  and balances, dynamic fields, `dev_inspect`/`dev_inspect_ptb`/`dry_run`
  (all `SimulateTransaction`), `execute`, `get_transaction`, checkpoint and
  gas-price reads, plus `created_objects`/`published_package`/
  `decode_return_value` helpers for reading effects.
- `events.rs`: `EventClient`, a GraphQL reader. **gRPC has no events query** —
  this is the one JSON-RPC capability the new API does not replace, so every
  `query_events` call site went here.

`SuiClientWrapper` now holds `client: ChainClient` + `events: EventClient`.
Changing that field's type is what made the migration exhaustive: the
compiler enumerated every call site.

`sui-rpc-api` and `sui-rpc` were added at the **existing** `framework/mainnet`
pin (they were already transitive deps in `Cargo.lock`), so the proto types
convert to/from `sui-types` for free — no version bump and no type-system
swap. `tonic 0.14` is pinned in `[workspace.dependencies]` to match the one
those crates build their `Status` on.

Migrated surfaces: `crates/sui-tx` (all PTB builders), `crates/move-publish`,
`tools/deployment-manager`, `tools/{trader,writer,exchange,deepbook-pool-test,trading-vault-smoke}`,
and services `keeper`, `mm-bot`, `option-scheduler`, `market-sim`,
`price-charting`, `cctp-relay`, `balance-monitor`, `hedge-signer`, `indexer`
(boot + `/progress` tip poll only — ingestion was always checkpoint-based).

**Publishing** no longer uses the JSON-RPC `transaction_builder().publish(..)`
helper (it does not exist on the gRPC client). Publishes are assembled
explicitly: `pt.publish_upgradeable(modules, deps)` + `transfer_arg(sender, cap)`,
and the resulting package/UpgradeCap are read off `changed_objects`.
Simple admin Move calls that used `transaction_builder().move_call(..)` with
`SuiJsonValue` args are likewise explicit PTBs now; `ChainClient::object_arg`
does the shared-vs-owned resolution that builder did internally.

**Dev-inspect** builds a gas-less `TransactionData` and calls
`SimulateTransaction` with checks disabled — verified live that a simulation
with an empty gas payment returns real decoded values.

### Config changes (operator-visible)

- `[sui] rpc_url` → **`[sui] grpc_url`** + **`[sui] graphql_url`**.
  `Secrets::resolve_rpc_url` → `resolve_grpc_url` / `resolve_graphql_url`.
  The old `rpc_url` key is still *parsed* (so an un-migrated secrets file
  loads instead of crash-looping a service) but nothing reads it, and the
  binaries log a warning when it is present.
- `options/<env>/sui-rpc` should now hold `{"grpc_url": …, "graphql_url": …}`.
  `render-secrets.sh` reads those keys.
- `options/<env>/cctp-relay` takes `grpc_url` (falls back to `rpc_url` for a
  smooth cutover). Still **required** — SO-320's no-public-fallback rule is
  unchanged.
- indexer / price-charting configs: `rpc_url` → `grpc_url` (accepted as a
  serde alias) plus `graphql_url` for price-charting's event watcher.
- `deploy --rpc` → `--grpc` (`--rpc` kept as a clap alias). It now falls back
  to the secrets file's `grpc_url` before the public default — the gap that
  broke the redeploy workflow.

**The public endpoints now work.** Under JSON-RPC the public default was
dead, so a missing override meant a broken service and the fleet depended on
a third-party provider. On gRPC/GraphQL the public fullnodes serve us, so an
absent `sui-rpc` secret is a normal configuration rather than an outage
waiting to happen. The overrides remain for rate limits and latency.

### Behaviour changes worth knowing

- **Move enum variants are readable again.** JSON-RPC's parsed content
  dropped the variant name, rendering `vault::Phase` as `{}`; the keeper
  carried a round-0 structural fallback to survive it. gRPC renders
  `{"@variant": "Settling"}`, so the phase is now read directly. The
  fallback is kept for old encodings.
- **price-charting's persisted event cursor changed format.** GraphQL cursors
  are opaque strings, not `(tx_digest, event_seq)` — and per the warning
  above, old cursors are not replayable. `watch_cursor` rows written before
  this change are detected (`cursor_ev != -1`) and dropped, and the watcher
  re-initialises from the stream tip. No migration needed.
- **Dynamic-field reads derive the field id client-side** wherever they can
  (Pyth `price_info`, the vault withdrawal queue), which is what the keeper
  already did to work around providers that don't serve a dynamic-field
  index.


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
