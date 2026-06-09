# 05 — Off-Chain Service Changes

**Files**: `services/indexer`, `services/api-service`, `services/mm-bot`,
`services/option-scheduler`, `crates/sui-tx`, `crates/protocol-types`, `crates/pricing`
**Depends on**: events/types from 01–03

## 1. Indexer

### 1.1 New events to decode (`src/event_types.rs`)

From doc 01: `CollateralizedWrite`. From doc 02: `RfqCreated`, `RfqBid`, `RfqSettled`,
`RfqExpiredUnsold`. From doc 03: the 13 vault events. Same BCS-decode + JSONB-payload
pattern as the existing 14 types; all land in `indexed_events` untouched.

### 1.2 Materializations (new migration)

```sql
CREATE TABLE rfqs (
  rfq_id          TEXT PRIMARY KEY,
  bucket_id       TEXT NOT NULL,
  origin          TEXT NOT NULL,           -- vault id or seller
  amount          NUMERIC(39) NOT NULL,
  reserve_premium NUMERIC(39) NOT NULL,
  deadline_ms     BIGINT NOT NULL,         -- updated on anti-snipe extensions
  best_premium    NUMERIC(39),
  best_bidder     TEXT,
  status          TEXT NOT NULL,           -- open | settled | expired_unsold
  winner          TEXT,
  net_premium     NUMERIC(39),
  position_id     TEXT
);

CREATE TABLE rfq_bids (
  rfq_id TEXT, bidder TEXT, premium NUMERIC(39), call_recipient TEXT,
  sequence BIGINT REFERENCES indexed_events(sequence), PRIMARY KEY (rfq_id, sequence)
);

CREATE TABLE vaults (
  vault_id TEXT PRIMARY KEY, underlying_type TEXT, settlement_type TEXT, share_type TEXT,
  round BIGINT, phase TEXT, current_bucket TEXT,
  total_shares NUMERIC(39), deployable NUMERIC(39), pending_deposits NUMERIC(39)
);

CREATE TABLE vault_rounds (
  vault_id TEXT, round BIGINT, bucket_id TEXT, strike NUMERIC(39), strike_scale SMALLINT,
  expiry_ms BIGINT, pps NUMERIC(39), aum NUMERIC(39), shares NUMERIC(39),
  premium_collected NUMERIC(39), mgmt_fee NUMERIC(39), perf_fee NUMERIC(39),
  finalized_at_ms BIGINT, PRIMARY KEY (vault_id, round)
);

CREATE TABLE vault_user_receipts (        -- powers "my deposits/withdrawals" UI
  object_id TEXT PRIMARY KEY, vault_id TEXT, kind TEXT,  -- deposit | withdraw
  owner TEXT, round BIGINT, amount NUMERIC(39), shares NUMERIC(39), status TEXT
);
```

### 1.3 `positions` table

`RfqSettled` mints a position with premium, like `WriteExecuted` — extend the positions
materializer to ingest both, with a `venue` column (`signed_quote` | `onchain_rfq`) and
`mm_account_id` nullable (RFQ winners are addresses, not Accounts; store winner in a new
`counterparty` column). `CollateralizedWrite` also inserts (venue `self_write`,
premium NULL).

### 1.4 GraphQL

Add queries mirroring the new tables: `vaults`, `vaultRounds(vaultId)`, `rfqs(status, origin)`,
`rfqBids(rfqId)`. The quoting-service is untouched (the on-chain RFQ bypasses it entirely);
the mm-bot and keeper consume these reads.

## 2. api-service

New routes (same JIT-GraphQL pattern as existing handlers):

```
GET /vaults                       → list w/ live pps, tvl, current round summary, APY (Ribbon formula
                                    over vault_rounds; computed here, not stored)
GET /vaults/:id                   → detail + config + phase + current bucket
GET /vaults/:id/rounds            → round history (the track-record endpoint)
GET /vaults/:id/receipts?owner=   → user's deposit/withdraw receipts + claimable status
GET /rfqs?status=open             → open auctions (mm-bot/dashboards poll this as a fallback
                                    to event subscription)
```

**DeepBook pool address** (parallel work, tentative): extend the pair catalog
(`src/catalog.rs`) so each (underlying, settlement) entry carries
`deepbook_pool_id: Option<String>`, surfaced in `/buckets` and `/vaults` responses. The
keeper reads it for `swap_proceeds`; the frontend uses it as the secondary trading venue
for `Coin<Call>`. Treat absence as "swap crank unavailable" until the integration lands.

## 3. mm-bot: on-chain bidder

New module `src/onchain_rfq.rs` beside the WS flow; both share `pricing.rs` unchanged.

1. **Discover**: subscribe to `RfqCreated` via the indexer stream (fallback: poll
   `/rfqs?status=open`). Filter to configured pairs.
2. **Price**: reuse `price_rfq` with `Side::Writer` semantics — the bot is *buying* the
   option, so its max bid = `mid × (1 − bid_markdown_bps/10⁴) × amount` (identical math to
   the WS quote path; one pricing brain, two venues).
3. **Bid policy** (config):
   ```toml
   [onchain_rfq]
   enabled = true
   initial_bid = "reserve_plus"   # reserve_plus | max | shaded
   shade_bps = 300                # initial bid = max_bid × (1 − shade)
   rebid = true                   # top up to max_bid when outbid (RfqBid events)
   max_concurrent_escrow = "5000000000"   # cap total USDC locked across auctions
   ```
   Escrow accounting matters: every live bid locks real USDC. Track
   `locked = Σ open bids where we are best` against the cap; refunds (we got outbid) release
   on the corresponding `RfqBid` event.
4. **Fund** bids from the bot's wallet or from its `Account` (PTB:
   `account::withdraw<S>` → `rfq::bid`); the wallet is simpler — the bot owner choice.
5. **Settle**: optionally crank `settle` for auctions it won (keeper does it anyway).

## 4. option-scheduler: vol-aware strike grid (v2)

### 4.1 Problem with the current grid

`build_strike_grid_for_pair` spaces strikes at a fixed `interval_pct` of spot, centered on
spot. Whether the 0.1-delta strike (≈ `exp(1.28·σ√τ)` ≈ 6–15% above spot depending on vol)
is on-grid is then an accident of the configured percentage and the vol regime. The vault
needs that strike to exist *by construction*, with only ~5 buckets (7 on BTC).

### 4.2 Design: z-ladder

Place strikes at fixed **standard-deviation multiples** instead of fixed percentages:

```
K_i = round_nice( S · exp(z_i · σ · √τ) )
```

with σ = realized vol (30d daily, `pyth-client::vol`) clamped to a per-pair
`[vol_floor, vol_ceiling]`, τ = bucket tenor, and per-pair ladders:

```
SUI (5):  z ∈ { 0.00, 0.65, 1.30, 1.95, 2.60 }
BTC (7):  z ∈ { −0.65, 0.00, 0.65, 1.30, 1.95, 2.60, 3.25 }
```

Properties: ATM is always present (traders), `z = 1.30` ≈ the vault's 0.1-delta target is
**always on-grid**, spacing adapts to the vol regime automatically (5% intervals in calm
markets, 12% in wild ones), BTC's extra strikes add one ITM and one far-OTM wing.

`round_nice(x)`: round to the increment `10^⌊log10(x)⌋⁻² × {1, 2.5, 5}` nearest below 1% of
x — strikes read like real exchange strikes ($117 500, not $117 483.91) while staying within
half a grid step of the z-target. Strikes must remain strictly increasing after rounding
(bump a colliding strike one increment up).

### 4.3 Code changes

- `strike_grid.rs`: new `build_z_ladder_for_pair(spot, σ, τ, ladder, decimals…) →
  Vec<(strike_u128, strike_scale)>` — keep the existing scale-picker logic (resolution target
  of 1000 chain units) but apply it to the *smallest interval between adjacent strikes*
  rather than a uniform interval. The grid is no longer uniform; that's fine on-chain —
  `create_bucket` takes one arbitrary strike per call.
- `sui-tx/src/tx/coin_pkg.rs::create_buckets`: accept `Vec<u128>` strikes instead of
  `(start, interval, count)`.
- `families.rs`/`config`: per-pair ladder + vol window/floor/ceiling config; σ source =
  Hermes history via `benchmark_at` (or candle file fallback).
- Keep the old percentage path behind config for test tokens.

### 4.4 Coverage alert

Emit a metric when the keeper logs `GridCoverageMiss` (doc 04 §3) or when, at creation time,
`z = 1.30`'s rounded strike drifts > half a step from target. Persistent misses mean the
ladder or vol clamp needs retuning.

## 5. `crates/sui-tx` additions

New builders (used by mm-bot, keeper, tests): `rfq_create`, `rfq_bid`, `rfq_settle`,
`rfq_settle_expired`, `vault_deposit`, `vault_claim_shares`, `vault_initiate_withdraw`,
`vault_complete_withdraw`, and one builder per crank (`crank_redeem`, `swap_proceeds`,
`finalize_round`, `select_bucket`, `open_rfq`, `settle_rfq`), plus a `pyth_update` helper
that prepends Hermes price-update calls to a PTB. Mirror the existing
`execute_write.rs` style (typed args, shared-object resolution via `shared_object_arg`).

## 6. `crates/protocol-types` additions

Rust mirrors of the new events (serde + BCS, like the existing event types) and the
`VaultConfig` struct, shared by indexer, keeper, api-service.

## 7. `crates/pricing` additions

```rust
pub fn norm_cdf_inv(p: f64) -> f64                  // Acklam's algorithm
pub fn call_delta(i: CallInputs) -> f64             // N(d1)
pub fn strike_for_delta(spot, sigma, t, r, delta) -> f64   // closed form (doc 04 §3)
```

With golden-vector tests against the existing `call_price_per_unit` (delta via bump-and-
reprice must match analytic to 1e-6).

## 8. Frontend (interface contract only — implementation out of scope)

The vault page needs exactly: `/vaults`, `/vaults/:id`, `/vaults/:id/rounds`,
`/vaults/:id/receipts?owner=`, plus PTB construction for the four user functions (deposit,
claim, initiate/complete withdraw) — same `sui-tx`-template patterns the existing earn page
uses. Live RFQ activity (open auctions, best bids) comes from `/rfqs` + `RfqBid` events for
a "current auction" widget.
