# solana-price-charting — Frontend Integration Guide

Base URL: `/{env}/solana-charts` (nginx route, mirror of the Sui `/charts`
prefix; WS upgrade enabled on the same prefix). The service listens on port
9011 inside the docker network.

All wire shapes are **byte-identical to the Sui price-charting service** —
the existing chart layer (Lightweight Charts panel, WS hook, `/pools`
listing) is drop-in against this base URL.

## Important: no trade/mid data flows yet

There is **no Solana order-book integration yet**, so nothing ingests
trades or order-book midpoints:

- `GET /pools` returns `[]`.
- `GET /bars` returns `{ "bars": [], "mids": [] }`.
- `GET /price-at` returns `{ …, "mid": null, "close": null }`.
- `/ws` accepts subscriptions and pushes nothing (pings only).

These are not error states — they're exactly what an empty database
produces, and the frontend already handles them (identical to a quiet pool
on Sui). When a Solana venue integration lands, data starts flowing through
the same endpoints with no contract change.

The **live** part today is `GET /vault-apy/:vault_id`, fed by the vault-APY
sampler (indexer + oracle driven — independent of the order-book gap).

## Endpoints

### `GET /health`

Plain-text `ok`.

### `GET /pools`

Array of pools that have trade data (currently always empty):

```json
[
  {
    "pool_id": "…",
    "bucket_id": "…",
    "last_price": 0.42,
    "last_trade_ms": 1760000000000,
    "volume_24h_base": 12.5,
    "watched": true
  }
]
```

Ids are base58 account pubkeys (on Sui they were 0x object ids — same field,
different id format; treat them as opaque strings).

### `GET /bars?pool_id=…&interval=…&from_ms=…&to_ms=…`

- `interval`: one of `1m`, `5m`, `15m`, `1h`, `4h`, `1d`.
- `from_ms`/`to_ms` optional; the range is clamped to the newest 1000 bars.
- 400 on unknown interval or an empty/inverted range.

```json
{
  "bars": [ { "t": 1760000000000, "o": 1.0, "h": 1.2, "l": 0.9, "c": 1.1, "v": 3.0 } ],
  "mids": [ { "t": 1760000000000, "m": 1.05 } ]
}
```

`t` is the bucket start (unix ms); fields map 1:1 onto Lightweight Charts
candlestick/line points. Empty buckets inside the range are carry-forward
filled (flat candle, `v: 0`); buckets before the first trade ever are
omitted. Currently both arrays are always empty (see above).

### `GET /price-at?pool_id=…&ms=…`

Last known price at or before `ms`:

```json
{ "pool_id": "…", "ms": 1760000000000, "mid": null, "close": null }
```

`mid` = last order-book midpoint, `close` = last traded price; either is
`null` when no data precedes `ms` (currently always, see above).

### `GET /vault-apy/:vault_id`

`vault_id` is the vault's base58 pubkey. Latest predicted-APY snapshot; the
realized series is served by the solana-indexer's `vaultApy` GraphQL query
(compose the two exactly as on Sui).

```json
{
  "vault_id": "…",
  "predicted": [
    {
      "t_ms": 1760000000000,
      "apy": 0.12,
      "apy_low": 0.05,
      "apy_high": 0.2,
      "assignment_prob": 0.07,
      "downside_round_yield": -0.01,
      "kind": "current",
      "horizon": 0,
      "confidence": 0.5
    },
    {
      "t_ms": 1760604800000,
      "apy": 0.15,
      "apy_low": 0.08,
      "apy_high": 0.22,
      "assignment_prob": 0.07,
      "downside_round_yield": -0.01,
      "kind": "forecast",
      "horizon": 1,
      "confidence": 0.6
    }
  ]
}
```

- `kind: "current"` (horizon 0) — this round, annualized from the live
  covered-call **auction** premiums (best bids on open auctions, net
  proceeds on settled ones); `confidence` is the settled fraction of the
  round's auctioned notional.
- `kind: "forecast"` (horizon 1..K) — Black–Scholes forecast for future
  rounds; `confidence` decays with horizon.
- `apy_low`/`apy_high` straddle `apy`; APYs are fractions (0.12 = 12%),
  clamped to ±5.0.
- `predicted: []` until the sampler has produced a snapshot for the vault
  (new vault, or inputs missing — e.g. config not indexed yet).

### `GET /ws`

Same protocol as Sui charts. Client → server:

```json
{ "type": "subscribe", "pool_id": "…", "interval": "1m" }
{ "type": "unsubscribe", "pool_id": "…", "interval": "1m" }
```

Server → client:

```json
{ "type": "bar", "pool_id": "…", "interval": "1m", "bar": { "t": …, "o": …, "h": …, "l": …, "c": …, "v": … } }
{ "type": "mid", "pool_id": "…", "interval": "1m", "point": { "t": …, "m": … } }
{ "type": "error", "message": "unknown interval 2h" }
```

On subscribe the server snapshots the current bar immediately if one exists
(currently never, since there are no trades). Until ingestion lands the
socket only sends protocol pings — keep the existing quiet-stream handling.
