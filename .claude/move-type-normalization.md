# Move type (`0x`) normalization

Coin/asset/Move type strings reach our backend in **two different forms** that
look equal to a human but are **not byte-equal**. Comparing them raw silently
fails. This bit us in SO-163: the indexer dropped every DeepBook pool because a
bucket's `call_type` and the pool's base asset never compared equal.

## The two forms

| Source | Example | `0x`? | Address padding |
|--------|---------|-------|-----------------|
| Chain `TypeName` — BCS-decoded event **payload** fields (e.g. `BucketCreated.asset_type` / `settlement_type` / `call_type`) | `9b72…ba86::tusdc::TUSDC` | **no** | as the chain stores it |
| Event **type string** — `event.type_.to_string()`, including generic params like `pool::PoolCreated<Base, Quote>` | `0x9b72…ba86::tusdc::TUSDC` | **yes** | as RPC renders it |
| Framework short types | `0x2::sui::SUI` | yes | **not** padded |

`AssetType` (`crates/protocol-types/src/asset.rs`) is a `#[serde(transparent)]`
`String` newtype. Its derived `PartialEq`/`Hash`/`Ord` are **byte-exact** — there
is no normalization in equality. So `AssetType("9b72…::tusdc::TUSDC")` ≠
`AssetType("0x9b72…::tusdc::TUSDC")`, and they hash to different buckets.

## The rule

**Before you compare, key a map by, or look up any Move/coin/asset type, run
both sides through the canonical form.** Never byte-compare a type that came
from a chain `TypeName` against one that came from an event type string (or
client input, or config).

- Backend: `AssetType::to_canonical()` or the free function
  `protocol_types::asset::canonicalize_move_type(&str)`. Canonical =
  `0x`-prefixed, lowercase, address left-padded to 64 hex. Idempotent.
- Frontend: `normalizeStructTag` (same contract).

```rust
// WRONG — raw byte compare across two sources
if pool.base_asset_type == bucket.call_type { … }
map.get(&pool.base_asset_type)            // map keyed by chain TypeName → miss

// RIGHT — canonicalize both sides
if pool.base_asset_type.to_canonical() == bucket.call_type.to_canonical() { … }
map.insert(bucket.call_type.to_canonical(), …);
map.get(&pool.base_asset_type.to_canonical());
```

## Also

- **Emit canonical (`0x`-prefixed) to clients and JSON-RPC.** `suix_getBalance`
  and PTB type-arg parsing reject the bare chain-`TypeName` form, and the
  frontend feeds our strings straight into those calls.
- A unit test that hand-builds both sides with the *same* literal will pass
  while production fails — make type tests use the real divergent forms (one
  chain `TypeName`, one `0x`-prefixed type string). See
  `services/indexer/src/worker.rs::deepbook_pool_resolves_across_0x_prefix_mismatch`.
