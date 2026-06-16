# api-service

HTTP backend for the frontend. Every read is a just-in-time GraphQL query to
the indexer — api-service holds no protocol state of its own. Holds no funds,
signs nothing — strictly a read/query layer.

## Run

```
cargo run -p api-service
```

Config: `services/api-service/config/config.toml`. By default binds
`127.0.0.1:9003` and reads protocol state via the indexer's GraphQL API at
`indexer_graphql_url` (default `http://127.0.0.1:9002/graphql`). Token
symbols/decimals come from the token-info service.

The indexer must be running and caught up for these endpoints to return
data — every row is sourced from a query against the indexer's materialized
views or event log.

## Endpoints

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Liveness — returns `"ok"`. |
| `GET` | `/buckets` | Active buckets, grouped by series. See shape below. |
| `GET` | `/positions?wallet=0x…` | `Position` objects held by a wallet (writer side). |
| `GET` | `/call-token-lots?wallet=0x…` | `WriteExecuted` purchase lots where the wallet was the call-token recipient (buyer provenance). |
| `POST` | `/dashboard/positions` | Enrich a wallet's written positions (SO-97). Body `{ "object_ids": ["0x…"] }`. Queries the indexer GraphQL `positions(objectIds)` and layers on catalog symbols/decimals + USD strike. Ids the indexer doesn't know yet are omitted (caller renders them degraded). The authoritative id list comes from the wallet (`getOwnedObjects ::position::Position`), not this service. |
| `GET` | `/indexer/progress` | Proxies the indexer's checkpoint-ingestion progress for the Debug page. |
| `GET` | `/events?wallet=0x…` | The wallet's activity feed (writes/buys/exercises/claims/deposits/withdraws), newest first, sourced from the indexer's `events(participant:)` query. `tx_hash` is `null` (digest not carried by `IndexedEvent`). |
| `GET` | `/options/metrics` | Stateless Black-Scholes greeks, implied vol, fair value & break-even for one call. Pure compute (no indexer/Pyth/DeepBook reads) — the frontend passes in live spot and the order-book mark. See shape below. |

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

## `GET /options/metrics` response shape

Black-Scholes greeks, implied vol, fair value, and break-even for a single
**call**. Stateless: every input is a query param, so api-service does no
indexer / Pyth / DeepBook reads — the frontend holds live spot (Pyth Hermes)
and the order-book mark (DeepBook `devInspect`) and passes them in.

### Query params

| Param | Type | Notes |
|---|---|---|
| `spot` | `number` | Current underlying price. |
| `strike` | `number` | Option strike. |
| `t_years` | `number` | Calendar time to expiry in years (e.g. 30 days → `30/365`). |
| `mark` | `number` | Observed option price — the order-book mid for a pre-trade quote, or the position's avg cost for break-even. |
| `r` | `number` | Optional. Annualized continuous risk-free rate. Defaults to `0` (matches a retail screen that ignores rates). |

**UNITS:** `spot`, `strike`, and `mark` MUST share one unit (e.g. settlement
per option, whole units). Greeks are returned **raw per-1.00**: `vega`/`rho` are
∂price per 1.00 (=100%) move — divide by 100 in the UI for per-1%. `theta` is
already **per calendar day** (annual θ ÷ 365), matching retail screens.

### Response

```json
{
  "implied_vol": 0.3367,
  "delta": 0.2274,
  "gamma": 0.002724,
  "vega": 99.26,
  "theta": -0.06359,
  "rho": 54.27,
  "break_even": 512.78,
  "fair_value": 12.78
}
```

`implied_vol` is `null` when there's no positive-vol solution — `mark` below the
no-arbitrage intrinsic, `mark ≥ spot`, or `t_years ≤ 0`. In that case greeks
fall back to the deterministic (σ=0) case so the row still renders, and
`fair_value` (the BS price re-priced at the solved IV) is also `null`.
`break_even` is always `strike + mark`. Example above:
`spot=387.70, strike=500, t_years=0.72, mark=12.78, r=0`.

## Architecture notes

- api-service is **stateless**: every endpoint answers from a just-in-time
  GraphQL query to the indexer, so it's safe to restart at any time and there
  is no event log to replay.
- Source of truth for symbols/decimals is the token-info service, fetched at
  startup. Restart the service after a deploy that adds tokens.
- If the indexer is unreachable, endpoints fail at request time (`502`)
  rather than serving stale state.
