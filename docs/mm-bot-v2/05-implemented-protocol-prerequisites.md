# mm-bot V2 protocol prerequisites — implemented (local, tested)

Status: **IMPLEMENTED locally, 2026-07-20; not yet deployed.** This
records what was built and the decisions made, in the style of
`docs/vault-curator-product.md`. Covers `00-plan.md` Phase 5 (contracts)
and the shared contract phase (§3a) of `03-bluefin-integration-plan.md` /
`04-deepbook-margin-integration-plan.md`. All Move-tested: 242 tests
across 11 packages green (36 new).

Standing product decisions recorded alongside this work:

- **mm-bot V2 trades and quotes ONLY from the trading vault.** The bot is
  the vault's curator and nothing else; its pool exists to make markets
  for the protocol without losing LPs money (ideally making them some).
- **Venue-neutral naming.** The trading vault is a standalone product;
  perps/external accounts serve different purposes in different
  strategies. Nothing in the vault is named "hedge": the primitives are
  the **external account**, its **equity oracle**, releases and returns.

## 1. Trading-vault: external-account primitives (`trading-vault`)

`TradingVault` gained an optional `ExternalAccount` (struct-level field —
this repo redeploys packages rather than upgrading, so layout changes are
fine):

- `set_external_account(AdminCap, vault, &OracleRegistry, account,
  equity_oracle: TypeName, budget_bps, daily_release_bps)` — admin-gated
  registration/rotation; the pinned equity-oracle witness must be
  allowlisted. `clear_external_account` only at zero exposure.
- `release_external<T>(vault, &CuratorCap, Appraisal, amount, clock)` —
  the only vault outflow that does not return in-transaction. Curator-
  gated, deposit-asset only, Open state only; **consumes a complete
  `Appraisal`** so both limits bind on true NAV at release time: total
  exposure ≤ `budget_bps` of NAV, tumbling-24h released ≤
  `daily_release_bps` of NAV. Pays only the registered address.
- `return_external<T>(vault, Coin<T>)` — accepted only when
  `ctx.sender()` IS the registered account (the co-signed sweep tx);
  reduces `exposure` (floored at zero — venue profit above cost).
- **Equity leg is mandatory**: `begin_appraisal` marks the appraisal
  `external_pending` whenever an account is configured; consumption
  aborts `appraisal_incomplete` (82 — already keeper-benign-classified)
  without `record_external_equity`, which only the PINNED and
  still-allowlisted oracle witness can call. Delisting the witness is an
  instant kill switch.
- `finalize_close` additionally requires zero exposure.
- New errors 100–104; events `ExternalAccountSet/Cleared`,
  `ExternalReleased` (with NAV), `ExternalReturned`.

Deviations from the plan docs: releases/returns are deposit-asset only
(Bluefin is USDC-collateral; DBM repatriation converts to quote before
withdrawing) and the rate window is a tumbling 24h window anchored at the
first release in a window, not a rolling one — simpler state, same bound.

## 2. `contracts/equity-oracle` — attested equity with guardrails

Keeper-posted equity book (`EquityBook`, one entry per vault id) minting
the vault's equity leg through the `EquityOracle` witness. Venue-neutral:
serves Bluefin AND DBM (or any venue) as the operator-attested path.

- Guardrails all on-chain, admin-parameterized: poster allowlist,
  `min_interval_ms` (default 60s), `max_delta_bps` per update (default
  20%), `max_age_ms` staleness at record time (default 5m, on top of the
  vault's own backstop).
- `seed_equity` (AdminCap) bypasses guardrails — registration, venue
  rotation, divergence recovery are governance acts. A poster cannot move
  a zero entry at all (bps-of-zero), so a zeroed mark heals only via
  governance.
- `record(vault, book, oracle_registry, &mut Appraisal, clock)` composes
  into any appraisal PTB.

## 3. `contracts/dbm-oracle` — computed equity (trustless sibling)

For readable venues: derives equity from a real `MarginManager` inside
the appraisal PTB — no operator input, no guardrails needed.
`equity = value(calculate_assets incl. locked order balances) −
value(calculate_debts)`, valued into the deposit asset via ordinary
`PriceAttestation` legs (assets floor, debts ceil — understated equity;
manager DEEP balances ignored, a further understatement). Binding checks:
`manager.owner()` must equal the vault's registered account, and the pool
must be the manager's own `deepbook_pool`. `record_no_debt` /
`record<…, DebtAsset>` split because `calculate_debts` aborts for a
never-borrowed manager.

- Deps: `deepbook_margin` + `deepbook` from MystenLabs/deepbookv3 `main`
  (git), pyth mainnet rev — the CANONICAL DeepBook, correct for margin
  (hedge venue ≠ options venue; the house publish has no margin layer).
- Package is edition **2024.alpha**: its tests use `extend module
  pyth::price_info` to add the `PriceInfoObject` test constructor (same
  trick deepbook_margin's own tests use). The e2e test drives a REAL
  manager (registry → margin pools → permissionless pool → margin enable
  → `margin_manager::new` as the external account → oracle-checked
  deposits) and settles doc 04 Phase 0 item 4: the getter surface IS
  callable from an external package.

## 4. Options core: exact-offset closure (calls + puts)

`close_offset(bucket, &mut Position, Coin<option>, clock)` nets a
writer's own coins against their position pre-expiry and frees collateral
(underlying for calls; `floor(amount × strike)` cash for puts — payout
rounding, dust stays, solvency proof unchanged; closed units also count
into put `total_redeemed` so cleanup stays reachable).

Queue mechanics — the load-bearing design: the closed slice is carved
from the position's range END (positions stay contiguous; only the
unexercised part may close, `cursor ≤ cut`) and becomes a **tombstone
interval** (`closed`, sorted/disjoint/≥ cursor, adjacent-merged). The
FIFO cursor **jumps tombstones** as it meets them (eagerly when flush
against one), and exercise capacity is
`total_written − cursor − closed_pending`. Supply, pooled collateral, and
per-position redeem overlap all stay exact — locked by conservation
tests. A fully-closed position zeroes in place;
`position::destroy_empty` disposes it.

## 5. Options core: spread collateral compression (calls)

`write_spread(short_bucket, &long_bucket, Coin<LongCall>, exercise_cash)`
writes covered-by-a-long-call instead of by underlying: escrow = the long
coins (equal-or-LOWER strike, equal-or-LATER expiry, same U/S pair) plus
EXACTLY `required_settlement(long_bucket, amount)` cash. Physical
settlement makes the cash leg mandatory — compression means the maker
never warehouses underlying (V2's JIT-spot alternative), not
collateral-free writing.

- The compressed range enters the FIFO tail, but the cursor **refuses to
  enter it** (`spread_unwind_required`, code 63) until anyone cranks
  `unwind_spread` — exercise the escrowed long into the short bucket's
  pool — after which range and position are indistinguishable from a
  physical write. The escrow guarantees the crank succeeds whenever the
  short bucket is live.
- Never-physicalized ranges are provably unexercised: `close_spread`
  (pre-expiry; burn back the full short coins, escrow returned, range
  tombstoned) and `redeem_spread_position` (post-expiry; escrow returned,
  long coins possibly still live). `redeem_position` and `close_offset`
  reject spread positions (code 68).
- Escrows are dynamic fields on the bucket keyed by range start;
  `cleanup_bucket` requires all spreads gone.

New core error codes 61–68; events `OffsetClosed`, `SpreadWritten`,
`SpreadUnwound`, `SpreadClosed`, `SpreadRedeemed`.

## 6. Follow-ups (explicitly out of this pass)

1. **Redeploy + activation**: publish equity-oracle (and, when the DBM
   venue is scheduled, dbm-oracle) via deployment-manager; allowlist
   witnesses; ids into `deployments.json`.
2. **Indexer/api**: new event variants (`OffsetClosed`, `Spread*`,
   `External*`, `EquityPosted`) are not yet in `ChainEvent`; unknown
   events are ignored until then.
3. **Keeper**: equity-attestation cron posting into `EquityBook`,
   reconciliation monitor (`exposure` vs releases−sweeps vs equity,
   `alert_id = "hedge-reconciliation"`), and the `release_external` /
   `return_external` PTB templates.
4. **hedge-signer service** (FROST / native-multisig co-signing + policy
   engine) — Phase 2 of docs 03/04; nothing here depends on it.
5. **options-adapter**: `appraise_*_position` marks positions as
   pool-backed; if the vault ever custodies a SPREAD position through the
   adapter, its appraisal would be wrong — gate or extend before the
   vault writes spreads.
6. Put-side spread compression (needs assignment-funded unwind fused into
   exercise) — deliberately out, per the plan's calls-only scope.
