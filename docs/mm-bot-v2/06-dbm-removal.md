# DeepBook Margin removal (SO-334)

Status: **DONE.** The DeepBook-Margin (DBM) hedge integration is gone from
contracts, services and frontend. Bluefin Pro is the sole planned hedge
venue. This records what was removed, what survived, and what it costs —
so the tradeoff isn't re-litigated from diffs.

## Why

DBM was added quickly and never had a clean test path. Three structural
reasons made it the wrong venue rather than merely an untested one:

1. **It could never cover this book.** A `MarginManager` hedges ONE base
   asset, and mainnet DBM has no BTC pair (SUI/USDC 5x, WAL and DEEP 3x).
   The desk derives markets from listed underlyings — TBTC among them.
   Bluefin lists BTC/ETH/SUI/SOL/DEEP/WAL.
2. **Its carry is always a cost.** Borrow APR is a kinked utilization
   curve (12%→62% between 80–90% util on USDC). Bluefin funding is one of
   V1's four revenue lines — shorts often EARN it.
3. **Service liveness became a margin-safety dependency.** Every DBM
   action is owner-sent, so every trade and every margin top-up needed the
   2-of-2 co-signing ceremony. A violent rally with the signer unavailable
   ends in liquidation. Bluefin's authorized wallet adjusts margin
   unilaterally.

## What was removed

- `contracts/dbm-oracle` (the computed-equity adapter), its publish step
  in deployment-manager, the `DbmOracle` witness from the activation
  allowlist PTB, and the `dbmOracle` records in `deployments.json` /
  `PackageInfo` / token-info-client.
- `mm-bot desk/dbm.rs` (the `deepbook_margin` `HedgeVenue`) and the
  `"deepbook_margin"` arm of `HedgeConfig::venue_specs`. `"paper"` is now
  the only kind that parses.
- The keeper's `[external.dbm]` blocks and the `DbmLegInfo` plumbing in
  `crates/sui-tx` `appraisal.rs`, plus the `dbm_oracle::record*`
  gas-station template legs.
- The frontend's `tx/dbm.ts`, `scripts/dbm-discovery.ts`, the DBM ids in
  `config.ts` and the `{kind:"dbm"}` external-equity plan.
- The hedge-signer's margin-perimeter policy: the Auto and Emergency
  tiers, the `Perimeter`/`Deposit`/`Borrow`/`Withdraw` call kinds, the
  borrow cap, and the `deepbook_margin_package` pin.

## What survived (venue-neutral, all still load-bearing)

- The trading-vault **external account** primitives: `set_external_account`
  / `release_external` / `return_external`, the budget + daily rate limit,
  the mandatory equity leg and `finalize_close`'s zero-exposure gate.
- `contracts/equity-oracle` — the keeper-attested `EquityBook` with its
  on-chain guardrails (poster allowlist, min interval, max delta,
  staleness). It is now the ONLY equity path.
- The keeper's Bluefin equity reader (`venue_equity::Bluefin`, SO-305).
- The hedge-signer's FROST substrate, `bluefin_proxy`, the Bluefin payload
  policy (login / authorize_account / withdraw / sui_tx) and the sweep
  classifier — which the Bluefin `sui_tx` path calls directly.
- Flash-exercise. It runs on our **house DeepBook spot** pools'
  zero-fee flash loans (`sui_tx::tx::deepbook::flash_exercise_call`), not
  the margin layer, and is untouched.

## What it costs

Three honest losses, accepted:

1. **Trustless hedge NAV → attested.** DBM's manager is a readable shared
   object, so equity could be computed inside the appraisal PTB with no
   operator input. Bluefin equity is keeper-attested, bounded by the
   equity-oracle guardrails. Same posture as every operator-attested
   input; the reconciliation monitor (`hedge-reconciliation`) is the
   detector.
2. **Zero basis risk → perp basis + funding variance.** DBM shorted the
   actual asset.
3. **Atomic exercise + hedge unwind → two-legged.** `00-plan.md` §5's
   "the exercise sale and hedge unwind are the same trade" was literally
   one PTB on DBM. On Bluefin the unwind is an async API call, so
   **unwind the perp BEFORE firing the exercise ladder**, not after —
   otherwise the book is transiently naked-short between legs.

## Consequence for the desk

`paper` is the only executable venue until `HedgeVenue::bluefin` lands
(TODO §3.3), and that is blocked on the Bluefin Phase-0 asks (docs 03
§Phase 0 — needs a human email). The V1 desk still quotes, books, limits
and attributes P&L exactly as before; only the hedge is simulated. Since
delta hedging is V1's primary revenue engine, the strategy stays
economically unvalidated until then — which was already true, as the DBM
venue ran as a monitored secondary and never traded.
