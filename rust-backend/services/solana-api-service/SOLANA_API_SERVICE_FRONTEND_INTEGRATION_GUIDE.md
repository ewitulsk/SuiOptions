# solana-api-service — Frontend Integration Guide

REST read layer for the Solana deployment. Same philosophy as the Sui
api-service: stateless, every request a JIT GraphQL query to
solana-indexer, dual numerics (scaled `f64` for display + raw decimal
`string` for precision/tx-building).

**Base URL:** `/{env}/solana-api` (nginx), e.g.
`https://sui-options.com/staging/solana-api`. Local dev:
`http://127.0.0.1:9003`.

## Deltas from the Sui api-service (read first)

- **Ids are base58 pubkeys**, not `0x…` hex: wallets, buckets, positions,
  vaults, auctions, mints. Compared byte-exact; no normalization.
- **`signature` replaces `tx_digest` / `tx_hash`** everywhere. The
  activity feed now carries a real per-event `signature` (the Sui feed's
  `tx_hash` was always `null`).
- **Token identity is the SPL mint**: `asset_mint` / `settlement_mint` /
  `option_mint` fields replace `asset_coin_type` / `settlement_coin_type`
  / `call_coin_type` / `option_coin_type`. There is no per-bucket "coin
  type" — match owned option tokens by `option_mint`.
- **`/auctions` replaces `/rfqs`.** The venue runs generalized auctions
  with a `mode` field (`swap` | `covered_call` | `cash_secured_put`);
  status `expired_unsold` → `unsold`; pure swaps have `bucket_id: null`.
  Bid history moves to `/auctions/:id/bids`.
- **`tradeable` semantics without pools**: no order book yet, so
  `tradeable = !cleaned && !invalidated && !expired`. There is no
  `deepbook_pool_id` field, and (unlike Sui) `invalidated` IS part of the
  gate.
- **PnL without `bm`**: no DeepBook, so `/dashboard/pnl` takes only
  `wallet`. Acquisition sources are `quote` (RFQ write) and `auction`
  (won venue auction) instead of `rfq`/`deepbook`. Exercises are marked at
  the option price from price-charting when available, **falling back to
  the bucket strike** (with no DEX ingestion yet, the fallback is what you
  get; `unpriced_exercise_amount` therefore stays `0`).
- **`POST /dashboard/positions`** takes `{"position_ids": [...]}` (was
  `object_ids`) and rows use `position_id` (was `position_object_id`).
  Positions are fresh-keypair accounts — the id is the account pubkey.
- **`mm_account_id` is nullable** on positions (collateralized writes have
  no MM counterparty).
- Invalid wallet params return empty lists (not errors); invalid path ids
  return 400/404. Indexer outage → 502.

All on-chain integers cross the wire as decimal strings in `*_raw`
fields; `*_ms` timestamps are JSON numbers (unix millis).

---

## `GET /health`

`200 "ok"`.

## `GET /buckets?exclude_expired=&exclude_invalidated=`

Bucket catalog grouped into series by
`(asset_mint, settlement_mint, expiry_ms, option_type)`.

```json
{
  "series": [
    {
      "asset_symbol": "TBTC",
      "asset_decimals": 8,
      "asset_mint": "So11111111111111111111111111111111111111112",
      "settlement_symbol": "TUSDC",
      "settlement_decimals": 6,
      "settlement_mint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
      "option_type": "call",
      "expiry_ms": 1782345600000,
      "expiry_iso": "2026-06-26T08:00:00Z",
      "buckets": [
        {
          "bucket_id": "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin",
          "strike": 85000.0,
          "strike_raw": "850",
          "strike_scale": 0,
          "option_mint": "opt1…",
          "total_written": 4.2,
          "total_written_raw": "420000000",
          "exercise_cursor": 1.0,
          "exercise_cursor_raw": "100000000",
          "fill_pct": 23.8,
          "invalidated": false,
          "tradeable": true
        }
      ]
    }
  ]
}
```

Unknown mints degrade: `*_symbol` falls back to the raw base58 mint,
`*_decimals` and every scaled field go `null` (raw strings survive).

## `GET /buckets/:bucket_id`

Narrow, poll-friendly single-bucket view with queue math. 404 for
unknown/cleaned buckets or a malformed id.

```json
{
  "bucket_id": "9xQeWv…", "asset_symbol": "TBTC", "asset_decimals": 8,
  "asset_mint": "So111…", "settlement_symbol": "TUSDC",
  "settlement_decimals": 6, "settlement_mint": "EPjF…",
  "strike": 85000.0, "strike_raw": "850", "strike_scale": 0,
  "expiry_ms": 1782345600000,
  "total_written": 4.2, "total_written_raw": "420000000",
  "exercise_cursor": 1.0, "exercise_cursor_raw": "100000000",
  "queued_ahead": 3.2, "queued_ahead_raw": "320000000",
  "fill_pct": 23.8,
  "option_mint": "opt1…", "option_kind": "call", "tradeable": true
}
```

## `GET /positions?wallet=<base58>`

Writer `Position` accounts by mint-time recipient, joined to buckets.

```json
{
  "positions": [
    {
      "position_id": "PoS1…", "bucket_id": "9xQeWv…",
      "asset_symbol": "TBTC", "asset_decimals": 8, "asset_mint": "So111…",
      "settlement_symbol": "TUSDC", "settlement_decimals": 6,
      "settlement_mint": "EPjF…", "option_mint": "opt1…",
      "option_kind": "call",
      "strike": 85000.0, "strike_raw": "850", "strike_scale": 0,
      "expiry_ms": 1782345600000,
      "range_start_raw": "0", "range_end_raw": "100000000",
      "total_written_raw": "420000000", "exercise_cursor_raw": "100000000",
      "premium_received_raw": "8500000",
      "mm_account_id": "Acc1…",
      "signature": "5j7s6NiJS3JAkvgkoc18WVAsiSaci2…",
      "minted_at_ms": 1760000000123
    }
  ]
}
```

`mm_account_id` is `null` for collateralized (non-quote) writes, where
`premium_received_raw` is `"0"`.

## `GET /call-token-lots?wallet=<base58>`

Purchase provenance ("lots") for owned option tokens — the
`WriteExecuted` / `PutWriteExecuted` events where the wallet is the
option-token recipient. Newest first. Current balances come from the
wallet's SPL token accounts, not from here.

```json
{
  "lots": [
    {
      "bucket_id": "9xQeWv…", "asset_symbol": "TBTC", "asset_decimals": 8,
      "asset_mint": "So111…", "settlement_symbol": "TUSDC",
      "settlement_decimals": 6, "settlement_mint": "EPjF…",
      "option_mint": "opt1…", "option_kind": "call",
      "strike": 85000.0, "strike_raw": "850", "strike_scale": 0,
      "expiry_ms": 1782345600000,
      "amount_raw": "100000000",
      "premium_paid_raw": "9000000",
      "seller_account_id": "Acc1…",
      "signature": "5j7s6NiJ…",
      "timestamp_ms": 1760000000123
    }
  ]
}
```

## `POST /dashboard/positions`

Enrich wallet-derived Position pubkeys. Ids the indexer doesn't know are
absent from the response (render those degraded, don't drop them).

Request:

```json
{ "position_ids": ["PoS1…", "PoS2…"] }
```

Response: `{ "positions": [ …same rows as GET /positions… ] }`.

## `GET /dashboard/pnl?wallet=<base58>`

FIFO cost-lot ledger per bucket. Acquire on quote writes (`gross_premium`)
and won option auctions (`gross_bid`); dispose on exercise (marked at the
option price, strike fallback) and expiry burn (proceeds 0). Display
units throughout.

```json
{
  "buckets": [
    {
      "bucket_id": "9xQeWv…",
      "asset_decimals": 8,
      "settlement_decimals": 6,
      "remaining_lots": [
        { "amount": 0.5, "cost": 45.0, "source": "quote", "acquired_at_ms": 1760000000123 },
        { "amount": 1.0, "cost": 80.0, "source": "auction", "acquired_at_ms": 1760000100456 }
      ],
      "realized_pnl": -12.5,
      "unpriced_exercise_amount": 0.0
    }
  ]
}
```

## `GET /indexer/progress`

Proxy of solana-indexer's slot ingestion status (Debug page). Note the
Solana coordinates: slots, not checkpoints.

```json
{
  "start_slot": 351230000,
  "current_slot": 351234567,
  "finalized_slot": 351234530,
  "rate_slots_per_sec": 2.4,
  "ms_since_last_slot": 410
}
```

## `GET /events?wallet=<base58>`

Activity feed, newest first. Types: `position_opened` | `exercise` |
`claim` | `burn` | `deposit` | `withdraw` | `auction_bid` |
`auction_settled`. Sides: `writer` | `trader` | `account`.

```json
{
  "events": [
    {
      "id": "evt-17-trader",
      "ts_ms": 1760000000123,
      "ts_iso": "2025-10-09T08:53:20Z",
      "type": "position_opened",
      "side": "trader",
      "status": "confirmed",
      "bucket_id": "9xQeWv…",
      "asset_symbol": "TBTC",
      "settlement_symbol": "TUSDC",
      "strike": 85000.0,
      "expiry_ms": 1782345600000,
      "amount": 1.0,
      "value_delta": -90.0,
      "value_unit": "TUSDC",
      "signature": "5j7s6NiJ…"
    }
  ]
}
```

New vs Sui: put/collateralized/burn rows; `auction_bid` (bidder outflow;
`value_delta` null — the bid event doesn't carry its mint) and
`auction_settled` (winner pays `gross_bid`, creator receives
`net_proceeds`); `signature` is always populated.

## `GET /options/metrics?spot=&strike=&t_years=&mark=&r=`

Pure Black-Scholes compute, unchanged from the Sui twin. All inputs share
one unit.

```json
{
  "implied_vol": 0.3492, "delta": 0.41, "gamma": 0.00002,
  "vega": 102.4, "theta": -18.3, "rho": 9.1,
  "break_even": 85950.0, "fair_value": 949.8
}
```

## `GET /auctions?status=&mode=&bucket=&creator=`

Replaces `/rfqs`. Filters: `status` (`open` | `settled` | `unsold`),
`mode` (`swap` | `covered_call` | `cash_secured_put`), `bucket` /
`creator` (base58). Scaled floats: `amount` in escrow-mint units, bid
fields in bid-mint units.

```json
{
  "auctions": [
    {
      "auction_id": "Auc1…",
      "mode": "covered_call",
      "bucket_id": "9xQeWv…",
      "creator": "VauLt…",
      "escrow_mint": "So111…", "escrow_symbol": "TBTC", "escrow_decimals": 8,
      "bid_mint": "EPjF…", "bid_symbol": "TUSDC", "bid_decimals": 6,
      "amount": 4.2, "amount_raw": "420000000",
      "notional": 357000.0, "notional_raw": "357000000000",
      "reserve_bid": 50.0, "reserve_bid_raw": "50000000",
      "best_bid": 75.0, "best_bid_raw": "75000000",
      "best_bidder": "Bidr…",
      "deadline_ms": 1760000000000,
      "max_deadline_ms": 1760000600000,
      "min_increment_bps": 25,
      "settle_authority": null,
      "status": "settled",
      "winner": "Bidr…",
      "token_recipient": "Rcpt…",
      "position_id": "PoS1…",
      "gross_bid_raw": "75000000",
      "fee_raw": "3000000",
      "net_proceeds_raw": "72000000",
      "bid_refunded": null
    }
  ]
}
```

Pure swaps: `mode: "swap"`, `bucket_id: null`, `position_id: null`.

## `GET /auctions/:auction_id/bids`

```json
{
  "auction_id": "Auc1…",
  "bids": [
    {
      "sequence": 17,
      "bidder": "Bidr…",
      "token_recipient": "Rcpt…",
      "bid_raw": "75000000",
      "previous_bid_raw": "70000000",
      "deadline_ms": 1760000000000
    }
  ]
}
```

## `GET /vaults` and `GET /vaults/:vault_id`

Vault list / detail. `share_mint` replaces the Sui `share_type`. The
detail endpoint adds a best-effort live account read; live fields are
`null` on the list endpoint and whenever the RPC read fails.

```json
{
  "vault_id": "VauLt…",
  "underlying_symbol": "TBTC", "underlying_decimals": 8,
  "underlying_mint": "So111…",
  "settlement_symbol": "TUSDC", "settlement_decimals": 6,
  "settlement_mint": "EPjF…",
  "share_mint": "Shr1…",
  "round": 3,
  "current_bucket": "9xQeWv…",
  "pps": 1.002, "pps_raw": "1002000000000",
  "tvl": 12.6, "tvl_raw": "1260000000",
  "total_shares_raw": "1200000000",
  "pending_deposits_raw": "57600000",
  "apy": 0.1097,
  "deposits_paused": false,
  "phase": "active",
  "mgmt_fee_pct": 2.0, "perf_fee_pct": 10.0,
  "min_strike_over_spot_pct": 2.0, "max_strike_over_spot_pct": 20.0,
  "round_ms": 604800000, "selling_window_ms": 86400000,
  "max_slice_amount_raw": "1000000000000", "max_open_rfqs": 4,
  "selling_ends_ms": 1759990000000,
  "current_expiry_ms": 1760000000000,
  "open_rfqs": 2, "open_swap_rfqs": 1,
  "total_fees": 0.03, "total_fees_raw": "3000000"
}
```

Delta vs Sui: the live balance fields (`deployable_raw`,
`proceeds_settlement_raw`, `withdrawal_pool_raw`, `claimable_shares_raw`,
`queued_withdraw_shares_raw`) are gone — on Solana those balances live in
PDA-seeded token accounts, not on the Vault account. New live fields:
`current_expiry_ms`, `open_swap_rfqs`. `pps` stays 1e12-scaled
(`PPS_SCALE` unchanged).

## `GET /vaults/:vault_id/rounds`

```json
{
  "vault_id": "VauLt…",
  "rounds": [
    {
      "round": 3,
      "bucket_id": "9xQeWv…",
      "strike": 65000.0, "strike_raw": "650", "strike_scale": 0,
      "expiry_ms": 1760000000000,
      "pps": 1.002, "pps_raw": "1002000000000",
      "aum_raw": "999000000", "shares_raw": "888000000",
      "premium_collected_raw": "77000000",
      "mgmt_fee_raw": "1000000", "perf_fee_raw": "2000000",
      "finalized_at_ms": 1760000001000
    }
  ]
}
```

## `GET /vaults/:vault_id/apy`

Realized (indexer `vaultApy`) + predicted (solana-price-charting,
best-effort — empty until it runs).

```json
{
  "vault_id": "VauLt…",
  "realized": [ { "t_ms": 1760000001000, "apy": 0.1097 } ],
  "predicted": [
    {
      "t_ms": 1760000060000, "apy": 0.12,
      "apy_low": 0.08, "apy_high": 0.16,
      "assignment_prob": 0.31, "downside_round_yield": 0.0021,
      "kind": "premium_yield", "confidence": 0.8
    }
  ]
}
```

## `GET /vaults/:vault_id/receipts?owner=<base58>`

Deposit/withdraw receipt aggregates with derived claimability. Receipts
are fresh-keypair accounts on-chain; the aggregate is keyed by owner.

```json
{
  "vault_id": "VauLt…",
  "receipts": [
    {
      "owner": "User1…",
      "round": 3,
      "kind": "deposit",
      "amount_raw": "57600000",
      "settled_raw": "0",
      "status": "pending"
    }
  ]
}
```

`status`: `pending` | `claimable` | `settled` — a deposit at round r
claims once pps[r−1] exists; a withdrawal at round r pays once round r
finalizes.

---

## Error conventions

| Case | Behavior |
|---|---|
| Invalid `wallet` query param | `200` with empty list |
| Malformed base58 path/query id | `400` (`404` for `/buckets/:id`) |
| Unknown bucket / vault | `404` |
| Bad `status` / `mode` filter value | `400` |
| solana-indexer unreachable | `502` |
| RPC / price-charting / derived-metrics outage | degraded `200` (live/predicted fields omitted) |
