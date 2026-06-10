# 04 — Vault Keeper (`services/vault-keeper`)

**Files**: new `rust-backend/services/vault-keeper/`, additions to `crates/sui-tx`,
`crates/pricing`
**Depends on**: 03 (vault contract), 05 §5 (tx builders)
**Trust model**: none. The keeper holds only a gas wallet. Every action is a public crank
the contracts validate; N keepers can run concurrently and merely waste gas racing.

## 1. Shape

Follow the option-scheduler's structure (boot → tick loop → classify/submit), it solves the
same problem:

```
services/vault-keeper/
├── src/
│   ├── main.rs          // config, boot checks, tick loop (tick_secs ~ 15)
│   ├── config.rs        // vault ids, feeds, RPC, gas budget, slice policy, vol source
│   ├── state.rs         // VaultView: fetch + decode vault/bucket/rfq objects via RPC
│   ├── planner.rs       // pure: VaultView + now → Vec<Action>   (unit-testable)
│   ├── strike.rs        // pure: delta-target strike selection (§3)
│   ├── slicing.rs       // pure: slice schedule (§4)
│   ├── submit.rs        // Action → PTB via sui-tx builders; error classification, retry
│   └── pyth.rs          // fetch Hermes update + build PriceInfoObject refresh calls
└── Cargo.toml           // deps: sui-tx, pricing, pyth-client, runtime-config, deployments
```

The **planner is a pure function** — that's the testing surface. `VaultView → Vec<Action>`
with actions:

```rust
enum Action {
    CrankRedeem { vault, bucket, n_remaining },
    SwapProceeds { vault, pool },
    FinalizeRound { vault },
    SelectBucket { vault, bucket },          // chosen by strike.rs
    OpenRfq { vault, bucket, slice_amount },
    SettleRfq { vault, rfq, bucket },
    SettleExpiredRfq { vault, rfq },
}
```

## 2. Tick logic (planner)

```
match vault.phase:
  Settling:
    if positions_head < positions_tail        → CrankRedeem (batch up to K per PTB)
    elif open_rfqs > 0                        → Settle{,Expired}Rfq for each due auction
    elif proceeds_settlement > 0 and swap due → SwapProceeds
    else                                      → FinalizeRound
  Active:
    if bucket expired (now ≥ current_expiry)  → CrankRedeem (flips phase on-chain)
    elif current_bucket is none               → SelectBucket(best candidate)
    elif now < selling_ends_ms                → OpenRfq per slicing schedule (§4)
    settle any rfq whose deadline passed      → SettleRfq
```

Pyth: cranks that take `PriceInfoObject`s need a fresh on-chain price — prepend the standard
Pyth `update_price_feeds` call (Hermes VAA via `pyth-client`) in the same PTB.

Race-safety: actions are idempempotent at the contract level; a lost race aborts with a clear
error code (`vault_wrong_phase`, `rfq_not_closed`, …). `submit.rs` classifies these as
`Benign` (another keeper won) vs `Retry` (transient RPC) vs `Fatal` (config bug) — same
pattern as `option-scheduler/src/roller.rs::classify_error`.

## 3. Strike selection (`strike.rs`)

The contract enforces a band (doc 03 §6); the keeper picks the point in the band. Target
delta Δ\* = 0.10:

```
K* = S₀ · exp( (r + σ²/2)·τ + z*·σ·√τ ),   z* = −N⁻¹(0.10) = 1.281552
```

Then choose the **smallest grid strike ≥ K\*** among live buckets of the right pair/expiry
(rounding up ⇒ delta ≤ target ⇒ conservative). `z*` is a constant — no inverse-CDF needed at
runtime, but add `norm_cdf_inv` (Acklam) to `crates/pricing` anyway; the backtester and any
future non-0.10 target need it, and use it to assert `delta(K*) ≈ 0.10` in tests.

σ source (config, in order): (1) realized vol from daily closes over `vol_window_days` (30)
via `pyth-client::vol::realized_vol` × `iv_ratio` (configured, default 1.15 — see doc 06 §5
for calibration); (2) static fallback per asset. Log the chosen σ, K\*, the snapped strike,
and its model delta with every `SelectBucket` — this is the calibration trail the backtest
validation consumes (doc 06 §9.4).

Edge: if no live bucket satisfies both K\* and the on-chain band, take the highest in-band
strike and log a `GridCoverageMiss` warning (feeds the scheduler-grid alert, doc 05 §4.4).

## 4. Slicing (`slicing.rs`)

Config:

```toml
[slicing]
slices = 4                 # number of RFQ slices per round
stagger_minutes = 90       # slice i opens at selling_start + i × stagger
retry_unsold = true        # re-open a failed slice once, immediately, same reserve
```

Rationale: staggering reduces the "all supply at one instant" footprint that depresses
auction clearing (the Ribbon-Friday effect), without needing any new mechanism. The planner
computes "slices that should be open by now but aren't" from `selling_ends_ms`,
`open_rfqs`, and remaining `deployable` — stateless, so keeper restarts and keeper races
are harmless. Slice size = `deployable / slices_remaining`, clamped to the contract's
`max_slice_amount`.

`vault-sim` sweeps `slices`/`stagger` (doc 06 §7) — ship the empirically chosen defaults.

## 5. Liveness & ops

- **Anyone can run it**: publish the binary + a public config (vault IDs, feeds). Document
  in the README that running it requires only a funded gas wallet.
- The team runs ≥ 2 instances on independent infra; community keepers add depth.
- Optional protocol-side incentive (decide pre-launch, default off): a config'd
  `crank_reward` in the vault paying the `finalize_round` caller a few cents' worth of
  underlying — pure liveness insurance, bounded, admin-capped.
- Metrics (reuse the `metrics`/Prometheus setup the other services use): actions submitted /
  won / lost-race, redeem backlog, time-to-finalize per round, grid-coverage misses,
  realized σ and strike per round.
- `--dry-run` mode like the option-scheduler: full planning, log intents, no submission.

## 6. Tests

- Planner unit tests: every phase/state corner (mid-settling restart, expired-but-unredeemed,
  unsold slices at window end, zero-deposit round, races where the view is stale).
- Strike: golden vectors (S, σ, τ → K\*), snap-up behavior, band-edge fallback.
- E2E on localnet via the control-panel stack: scheduler creates buckets → users deposit via
  test client → keeper runs a full round against a scripted bidder bot → assert PPS, fees,
  receipts, withdrawals (extends `rust-backend/tests`).
