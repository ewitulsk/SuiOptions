# Covered-Call Vault — Full Implementation Guide

> ## ⚠️ DEPRECATED (SO-332)
>
> **This product is retired.** `contracts/vault` is no longer published,
> the keeper no longer cranks it, the scheduler no longer provisions
> vaults, and its read APIs and UI are unrouted. See
> [`contracts/vault/DEPRECATED.md`](../../contracts/vault/DEPRECATED.md)
> for exactly what was turned off and how to revive it.
>
> This guide is kept as the design record. The live curated-vault product
> is [`docs/vault-curator-product.md`](../vault-curator-product.md)
> (`contracts/trading-vault`). Docs 01–02 (contract modularization,
> on-chain RFQ) still describe live code; docs 03–08 describe the retired
> product. The `vault-sim` crate and `tools/backtester` (doc 06) were
> removed in SO-452 — see `docs/mm-bot-v2/08-backtesting-framework.md`
> for the replacement `crates/backtester`.

**Status**: Design / implementation guide (v1)
**Scope**: Everything required to ship automated covered-call vaults (SUI-C, wBTC-C) on top of
the existing options protocol — on-chain RFQ, contract modularization, the vault itself, a
permissionless keeper, off-chain service changes, and the backtesting engine (`vault-sim`).

The companion protocol spec is [`options-protocol-spec.md`](../options-protocol-spec.md).
This guide assumes the protocol as **implemented** (per-bucket fungible `Coin<Call>`,
`strike/strike_scale` ratio model), not as originally spec'd.

---

## Document map

| Doc | Component | New or changed code |
|-----|-----------|---------------------|
| [01-contract-modularization.md](01-contract-modularization.md) | Refactor `bucket.move` so writing/minting is a composable core that multiple venues (signed-quote RFQ, on-chain RFQ, future auctions) share | `contracts/sources/bucket.move`, `errors.move`, `events.move` |
| [02-onchain-rfq.md](02-onchain-rfq.md) | On-chain RFQ: vault (or anyone) escrows collateral, emits an RFQ event, MMs bid premium on-chain, anyone settles against the best bid | new `contracts/sources/rfq.move` |
| [03-vault-contract.md](03-vault-contract.md) | The vault: Ribbon-style rounds, share token, deposit/withdraw receipts, fees, Pyth guardrails, permissionless cranks | new `contracts/sources/vault.move`, `oracle.move`; Pyth dep in `Move.toml` |
| [04-vault-keeper.md](04-vault-keeper.md) | Trustless keeper service — anyone can run it; it only cranks public functions | new `rust-backend/services/vault-keeper` |
| [05-offchain-services.md](05-offchain-services.md) | Indexer events/tables, api-service endpoints (incl. tentative DeepBook pool address), mm-bot on-chain bidder, option-scheduler vol-aware strike grid (5 strikes; 7 for BTC) | `indexer`, `api-service`, `mm-bot`, `option-scheduler`, `sui-tx`, `protocol-types` |
| [06-vault-sim.md](06-vault-sim.md) | Backtesting engine: math, traits, data plan, Monte Carlo, validation | new `rust-backend/crates/vault-sim`, `rust-backend/tools/backtester`; small additions to `crates/pricing` |
| [07-testing-and-rollout.md](07-testing-and-rollout.md) | Test plan, invariants, audit surfaces, phased rollout | — |

---

## Product summary

Per-asset covered-call vault, weekly cadence aligned to the option-scheduler's bucket families:

1. Users deposit underlying (SUI or wBTC). Deposits queue until the next round.
2. At each roll, the vault selects the bucket closest to a **0.10-delta** strike (snapped *up*
   on the grid) and sells calls on its deployable balance through the **on-chain RFQ**:
   the vault escrows underlying slices, MMs bid premium, the best bid wins, the vault
   receives the `Position` + premium, the winner receives `Coin<Call>`.
3. At expiry, the vault redeems its positions: unexercised underlying comes back; exercised
   amounts come back as `strike × amount` USDC, which is converted back to underlying through
   an **on-chain swap auction** (`swap_auction.move`, doc 03 §7.3) — MMs bid underlying for
   the proceeds, and the winning rate must clear a fresh Pyth band at settle. (This replaces
   the original DeepBook plan, which assumed a `Pool<Underlying, Settlement>` the protocol
   never mints.)
4. Round P&L rolls into the share price; Ribbon-style fees (2% mgmt / 10% performance,
   profitable rounds only) go to the protocol treasury; queued deposits/withdrawals process.

Everything in step 2–4 is a **permissionless crank**: the on-chain state machine enforces
correctness; the keeper is just a scheduler anyone can run.

### Why the math is tractable (front-of-queue property)

The protocol is American-style, physically settled, FIFO-assigned via the bucket cursor.
Because the vault writes at bucket creation it occupies `[0, Q)` — the **front** of the
exercise queue. Two consequences (proved in [06-vault-sim.md §2](06-vault-sim.md)):

- The vault's worst case is exactly the textbook covered call `min(S_T, K) + p` (full
  exercise iff ITM). Partial/behavioral under-exercise by holders only adds upside.
- Early exercise **weakly helps** the writer (the exerciser forfeits remaining time value).

So the conservative bound for all vault math is the plain covered call, and the FIFO/American
features only ever improve on it.

---

## Architecture after this guide

```
                       ┌───────────────────────────────────────────────┐
                       │                  Sui chain                    │
                       │  bucket.move ──── write core (refactored)     │
                       │     ▲    ▲                                    │
                       │     │    └── rfq.move (on-chain RFQ auction)  │
                       │     │              ▲          ▲               │
                       │  execute_write     │ bids     │ settle        │
                       │  (signed quotes,   │          │               │
                       │   unchanged)    vault.move ───┘               │
                       │                 (rounds, shares, fees,        │
                       │                  Pyth guardrails)             │
                       └───────▲──────────────▲──────────────▲─────────┘
                               │              │              │
            signed-quote RFQ  │   on-chain bids             │ cranks (permissionless)
                               │              │              │
   retail ──► quoting-service ─┘          mm-bot ◄── RfqCreated events ──┐
                                          (new on-chain bidder)          │
                                                                         │
   indexer ──► postgres ──► graphql ──► api-service ──► frontend         │
      │  (new: vault/rfq events, tables, endpoints, deepbook pool)       │
      └───────────────► vault-keeper (new service; anyone can run) ──────┘

   option-scheduler: vol-aware strike grid (5 strikes; 7 on BTC), creates buckets weekly
   vault-sim + backtester: offline; validates strategy before and after launch
```

## Build order and dependencies

```
Phase A (parallelizable):
  A1. crates/pricing additions (norm_cdf_inv, delta, strike-from-delta)   [05, 06]
  A2. vault-sim crate + backtester CLI                                    [06]
  A3. bucket.move modularization                                          [01]

Phase B (needs A3):
  B1. rfq.move                                                            [02]
  B2. option-scheduler strike-grid v2                                     [05]

Phase C (needs B1):
  C1. vault.move + oracle.move (+ share-coin codegen reuse)               [03]
  C2. mm-bot on-chain bidder                                              [05]
  C3. indexer + api-service support for rfq events                        [05]

Phase D (needs C1):
  D1. vault-keeper service                                                [04]
  D2. indexer + api-service support for vault events                      [05]
  D3. frontend vault page (out of scope here; api shapes provided)

Phase E:
  E1. localnet e2e, testnet shadow run, calibration vs vault-sim          [07]
```

`vault-sim` (A2) intentionally has **zero dependency** on the new contracts — start it
immediately; its output decides launch parameters (delta target, fees, slice policy,
premium denomination) before the contracts are finished.

## Decisions already made (from product discussions)

| Question | Decision |
|---|---|
| Strategy | Weekly ~0.10-delta covered calls |
| Assets | SUI/USDC and wBTC/USDC |
| Sale mechanism | On-chain RFQ (open ascending auction with escrowed bids); architecture keeps the signed-quote path and future sealed auctions pluggable |
| Slicing | Vault may split inventory into multiple RFQ slices per round |
| Accounting | Ribbon-style rounds; queued deposits; two-step withdrawals; underlying-denominated shares |
| Fees | 2% annualized mgmt + 10% performance, charged only on profitable rounds; parameterized |
| Keeper | Fully permissionless — no trusted admin in the round lifecycle |
| Premium denomination | Backtest parameter; on-chain vault has a config flag; pick empirically |
| Strike grid | ~5 buckets per expiry (7 for BTC), vol-aware spacing so the 0.1-delta strike is on-grid |
| Proceeds conversion | On-chain swap auction (`swap_auction.move`, doc 03 §7.3) — a Pyth-bounded reverse auction, **no DEX dependency**. (DeepBook is still used only as the optional frontend trading venue for `Coin<Call>`.) |
| Backtest stack | Rust |
| Premium model | Black-Scholes + proxy IV surface + MM haircut, swept as scenarios |
| Exercise model | Scenario-swept policies; rational-at-expiry is the conservative anchor |
