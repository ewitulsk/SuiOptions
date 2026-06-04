# api-service

HTTP backend for the frontend. Subscribes to the indexer's WS fanout,
maintains an in-memory view of protocol state, and serves it via REST.
Holds no funds, signs nothing — strictly a read/query layer.

## Run

```
cargo run -p api-service
```

Config: `services/api-service/config/config.toml`. By default binds
`127.0.0.1:9003`, subscribes to `ws://127.0.0.1:9001/`, and reads
`deployments.json` from the workspace root for the testnet network.

The indexer must be running for `/buckets` to return anything — every
bucket the api-service knows about came from a `BucketCreated` event
the indexer ingested and forwarded.

## Endpoints

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Liveness — returns `"ok"`. |
| `GET` | `/buckets` | Active buckets, grouped by series. See shape below. |
| `POST` | `/dashboard/positions` | Enrich a wallet's written positions (SO-97). Body `{ "object_ids": ["0x…"] }`. Proxies the indexer GraphQL `positions(objectIds)` query and layers on catalog symbols/decimals + USD strike. Ids the indexer doesn't know yet are omitted (caller renders them degraded). The authoritative id list comes from the wallet (`getOwnedObjects ::position::Position`), not this service. |

CORS is configured from `allowed_origins` in the config file. Defaults
allow `http://localhost:5173` (Vite dev server).

## `GET /buckets` response shape

Buckets are grouped into **series** keyed by `(asset_type,
settlement_type, expiry_ms)`. Within a series, every bucket is a distinct
strike — that's the level a user picks from when composing a trade.

Numeric fields ship in two flavors:

- **Scaled** (`f64`) — divided by the relevant token's decimals.
  Display-ready (`$85,000.00`, `4.2 BTC`).
- **Raw** (`string`) — the on-chain integer in atomic units, sent as a
  string to preserve u64/u128 precision. Required when building a
  transaction off this data.

Symbols and decimals come from `deployments.json`. A bucket whose coin
type isn't in the catalog falls back to the raw Move type string as its
`*_symbol`, with `*_decimals: null` and `null` scaled fields — the bucket
is still visible but flagged as un-renderable.

### Example

```json
{
  "series": [
    {
      "asset_symbol": "TBTC",
      "asset_decimals": 8,
      "settlement_symbol": "TUSDC",
      "settlement_decimals": 6,
      "expiry_ms": 1782345600000,
      "expiry_iso": "2026-06-26T08:00:00Z",
      "buckets": [
        {
          "bucket_id": "0x9c2b…42a1",
          "strike": 85000.0,
          "strike_raw": "85000000000",
          "total_written": 4.2,
          "total_written_raw": "420000000",
          "exercise_cursor": 1.0,
          "exercise_cursor_raw": "100000000",
          "fill_pct": 23.8
        }
      ]
    }
  ]
}
```

### Field reference

#### `SeriesDto`

| Field | Type | Notes |
|---|---|---|
| `asset_symbol` | `string` | From `deployments.json`. Falls back to raw Move type if unknown. |
| `asset_decimals` | `number \| null` | `null` when the coin type isn't in the catalog. |
| `settlement_symbol` | `string` | Same fallback behavior. |
| `settlement_decimals` | `number \| null` | |
| `expiry_ms` | `number` | Unix milliseconds. Safe in JS through year 2255. |
| `expiry_iso` | `string` | Pre-formatted ISO-8601 UTC, e.g. `"2026-06-26T08:00:00Z"`. |
| `buckets` | `BucketDto[]` | Sorted ascending by `strike`. Unknown-decimals buckets sink to the end. |

#### `BucketDto`

| Field | Type | Notes |
|---|---|---|
| `bucket_id` | `string` | Hex-encoded 32-byte Sui object id. |
| `strike` | `number \| null` | Strike in settlement whole units. `null` if settlement decimals unknown. |
| `strike_raw` | `string` | Raw u64 in settlement atomic units. |
| `total_written` | `number \| null` | Underlying whole units written into the bucket. |
| `total_written_raw` | `string` | Raw u128. |
| `exercise_cursor` | `number \| null` | Underlying whole units exercised. |
| `exercise_cursor_raw` | `string` | Raw u128. |
| `fill_pct` | `number \| null` | `100 * exercise_cursor / total_written`. `0.0` when nothing written. `null` when underlying decimals unknown. |

## Architecture notes

- The api-service is a **read model** built from indexer events. It owns
  no persistence and is safe to restart at any time — the indexer's WS
  snapshot replays missed events.
- Source of truth for symbols/decimals is `deployments.json`, read once
  at startup. Restart the service after a deploy that adds tokens.
- The WS snapshot the api-service receives at connect is bounded by the
  indexer's `recent_log_capacity` (default 1024 events). Once the
  indexer's history exceeds that, a fresh api-service boot may miss
  older `BucketCreated` events. Tracked separately — see SO-XX.
