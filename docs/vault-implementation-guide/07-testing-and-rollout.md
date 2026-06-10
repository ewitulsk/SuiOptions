# 07 — Testing, Audit Surfaces, Rollout

## 1. Test pyramid

### 1.1 Move unit tests (per doc)

- **01**: write-core regression (WriteExecuted byte-identical), self-write round-trip
  conservation, venue-interleaving supply invariant.
- **02**: full auction lifecycle, refunds, anti-snipe, reserve/increment enforcement,
  expiry/invalidation recovery paths, double-settle impossibility.
- **03**: share math in isolation (test-only pps setter), every crank's phase gating, the
  §11 invariant list, multi-round randomized sequences (deposit/withdraw/crank fuzz),
  genesis and empty-vault edges, receipt round-convention test.

### 1.2 Rust unit tests

- `pricing` additions: `norm_cdf_inv` round-trips `norm_cdf` to 1e-9 over (−6, 6);
  `strike_for_delta` → `call_delta` ≈ target.
- keeper `planner.rs`/`strike.rs`/`slicing.rs`: pure-function tables (doc 04 §6).
- scheduler `build_z_ladder_for_pair`: ladder hits z-targets within half a nice-rounding
  step; strictly increasing; scale-picker resolution preserved.
- mm-bot `onchain_rfq`: escrow cap accounting under concurrent auctions; rebid logic.
- `vault-sim`: property tests + golden vectors (doc 06 §9.2).

### 1.3 Localnet e2e (extends `rust-backend/tests`, driven via control-panel stack)

Script one **full vault round** with test tokens:

1. deploy contracts; scheduler creates a z-ladder bucket family;
2. three users deposit; keeper (real binary) runs genesis finalize → select → open 2 RFQ
   slices; a scripted bidder bot places competing bids incl. a snipe (assert extension);
3. settle; advance clock past expiry; an exerciser exercises 40% of the bucket;
4. keeper cranks redeem/swap(stub)/finalize; users claim shares / complete withdrawals;
5. assert: PPS, fee transfer to treasury, receipt payouts — **and** feed the same inputs to
   `backtester` for the golden-file ledger diff (doc 06 §9.3).

Repeat as failure-mode variants: zero bids all round; bucket invalidated mid-round; keeper
killed and restarted mid-settling; two keepers racing.

## 2. Audit surfaces (new, beyond the existing §9.3 list in the protocol spec)

1. `write_collateralized` being public — the full-collateralization safety argument (doc 01 §5).
2. RFQ escrow lifecycle: no path strands the bid escrow or the underlying escrow
   (settle/settle_expired total coverage); refund push-transfer correctness.
3. Vault accounting: pps set-once, fee cap (fees can't push pps below pps_prev), queue
   ordering, receipt round convention, withdrawal_pool solvency.
4. Oracle usage: feed-ID pinning, staleness, confidence gate; strike band and reserve floor
   integer math (cross-scale comparisons).
5. Phase machine liveness: prove no reachable state requires `AdminCap` to advance.
6. Shared-object deletion of `RfqAuction`; `ObjectTable` position FIFO under crank races.
7. Pyth dependency pinning and upgrade story in `Move.toml`.

## 3. Rollout

1. **Backtest gate** (no chain dependency): vault-sim validation milestones 1–3
   (doc 06 §9.4) pass; launch parameters chosen from the sweep report.
2. **Testnet**: full stack (scheduler v2 grid, vault, keeper, mm-bot bidder) on test tokens;
   team-run bidder provides floor liquidity; ≥ 3 full rounds clean, including one forced
   ITM expiry and one zero-bid round.
3. **Audit** of contract diffs (01–03) with the §2 list.
4. **Mainnet soft launch**: deposit cap via `pause/config`; team + ≥ 1 external MM on the
   bidder; keeper run by team ×2 + published binary for community keepers.
5. **Shadow calibration**: 4+ weeks of forward shadow test (doc 06 §9.4.4) comparing
   realized auction premiums to the model; recalibrate; then raise caps.
6. **Track record**: `/vaults/:id/rounds` is the public, indexer-derived round history from
   day one — the marketing asset compounds automatically.

## 4. Suggested ticket breakdown (matches the build order in README)

| # | Ticket | Size |
|---|---|---|
| A1 | pricing: norm_cdf_inv, delta, strike_for_delta | S |
| A2a | vault-sim: types, cursor, ledger + property tests | M |
| A2b | vault-sim: iv/premium/sale/exercise impls + engine loop | M |
| A2c | backtester CLI: data loaders, scenario sweep, reports | M |
| A2d | data fetch scripts + Ribbon/Deribit validation runs | M |
| A3 | bucket.move modularization + regression tests | S |
| B1 | rfq.move + tests | M |
| B2 | scheduler z-ladder + create_buckets Vec<u128> | S |
| C1a | oracle.move + Pyth dep | S |
| C1b | vault.move: types + user functions + share coin publish flow | M |
| C1c | vault.move: lifecycle cranks + invariant tests | L |
| C2 | mm-bot onchain bidder | M |
| C3 | indexer: rfq events/tables/graphql; api: /rfqs | M |
| D1 | vault-keeper service | M |
| D2 | indexer: vault events/tables; api: /vaults*; deepbook_pool catalog field | M |
| E1 | localnet e2e + golden-file cross-validation; testnet runbook | M |
