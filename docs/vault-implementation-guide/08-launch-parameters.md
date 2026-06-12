# 08 — Launch-Parameter Decision Memo (T-SUI-C)

**Date**: 2026-06-12 · **Inputs**: `tools/backtester/scenarios/{sui_launch,sui_mc,sui_stress}.toml`
with the measured calibration (vrp 1.19, skew −600 bps central / −1100 bps pessimistic,
auction haircut 15–35%, all from free data — see `tools/backtester/README.md`).
Reproduce with:

```
backtester sweep --scenario scenarios/sui_launch.toml --out out/sui_launch --rank apy_p5
backtester sweep --scenario scenarios/sui_mc.toml     --out out/sui_mc     --rank apy_p5
backtester sweep --scenario scenarios/sui_stress.toml --out out/sui_stress --rank apy_p5
```

## 1. Decision

| Parameter | Launch value | Where it lives |
|---|---|---|
| Delta target | **0.20** | keeper `TARGET_DELTA` (keeper config / `services/keeper/src/main.rs`) |
| Strike grid | z-ladder `SUI_LADDER`, σ-clamped [0.3, 2.0] | scheduler `[pairs.grid]` |
| Slices / stagger | **4 / 90 min** | keeper `[vaults.slicing]` |
| Premium policy | **at_roll** (swap proceeds, compound in kind; `hold_premium_in_settlement = false`) | vault config |
| Fees | **2% / yr mgmt + 10% perf** (200 / 1000 bps) | vault config |
| Reserve floor | **30–50 bps** of slice spot-notional (`min_reserve_premium_bps`) | vault config |
| Swap slippage cap | **50 bps** | vault config |
| Launch gate (off-model) | ≥ 3 committed bidders incl. our mm-bot; small TVL cap | ops |

## 2. Why Δ = 0.20 and not the headline 0.10

Two compounding effects punish low deltas in the *as-built* system:

1. **Grid snap-up.** The keeper snaps **up** from K\*(0.10Δ) to the next z-ladder
   strike, which at SUI vol regularly lands near 0.03–0.05Δ. A ~0.03Δ weekly
   call on SUI is worth only ~10–15 bps of notional — barely above the old
   10 bps reserve *before* the measured 25% auction haircut.
2. **Auction depth.** With a lone bidder (Poisson mean 1), the MC shows
   **38–48% of slices going unsold** at Δ ≤ 0.10, versus ≤ 5.5% with mean-3
   bidders. Thin premium + reserve floor + haircut = no trade.

MC central case (bootstrap, 2,000 × 3-yr paths, haircut 2500, mean-3 bidders,
at_roll, 4 slices, 2/10 fees):

| Δ target | premium/round (p50) | call-away | unsold | APY(USD) mean | APY(USD) p5 | P(<HODL) |
|---|---|---|---|---|---|---|
| 0.05 | 17 bps | 4.8% | 20.9% | −14.8% | −72.1% | 0.96 |
| 0.10 | 19 bps | 4.5% | 5.5% | −15.2% | −72.1% | 0.96 |
| **0.20** | **63 bps** | **9.8%** | **5.0%** | −22.8% | **−72.0%** | 0.94 |

Δ = 0.20 triples the premium take for the same tail (p5 is the asset's tail,
not the strategy's — see §4), with call-away at its designed N(d2) ≈ 10%.
Δ = 0.10's nominal advantage in mean APY is an artifact of selling almost
nothing (and at 0.10 the snap-up makes the *effective* delta ≈ 0.03–0.05).
The replay on real 2023→2026 SUI history (a net −35% window) goes further
and ranks Δ = 0.30 with USD-holding premium policies first (+33–34%/yr USD)
— in a bear, selling more and keeping the proceeds in USD both pay — but
we don't launch on one regime's replay.

## 3. Auction parameters: the depth cliff and the reserve

The single most important launch variable is **bidder count**, not any
on-chain parameter:

| E[bidders] (Δ=0.20, haircut 2500) | premium/round | unsold |
|---|---|---|
| 1 | 21 bps | 36.8% |
| 3 | 63 bps | 5.0% |
| 6 | 84 bps | 0.2% |

Hence the off-model gate: the C2 mm-bot plus at least two external MMs
bidding before real TVL. On the reserve floor: at Δ = 0.20 the fair clearing
premium is ~60–100 bps of notional even after the haircut, so a **30–50 bps
reserve** bounds the malicious-keeper / lone-bidder leak (doc 03 §6,
keeper README §1) at roughly half the honest premium without blocking real
fills. Do **not** pair a ≥ 50 bps reserve with a 0.10Δ target — the snapped
grid pick prices below it and every quiet auction would expire unsold.

Slices 4 vs 1 moves results < 1 pt everywhere; keep 4 (a failed slice is a
quarter of the round, not all of it). Premium policy is **not** a rounding
choice: `hold_usdc` beats `at_roll` by ~6 pts/yr in MC mean and by far more
on the bear replay (USD held through declines), at the cost of carrying a
mixed-asset book instead of a pure in-kind product. We launch `at_roll`
because it matches the product's "income in SUI" framing and the
swap-then-finalize contract flow — but the team should treat
`hold_premium_in_settlement = true` as a live alternative, not a tweak.

## 4. What the Monte Carlo actually says (read before quoting APYs)

- The **p5 ≈ −72% (USD, 3yr)** is SUI itself: max-drawdown p95 is ~98% on
  bootstrapped paths and the vault holds SUI. The covered call neither
  causes nor fixes that tail.
- **P(underperform HODL) ≈ 0.94–0.96**: on bootstrapped SUI (fat upside
  tails), capping the +30% weeks costs more than 60 bps/round earns. And
  on the actual 2023→2026 replay the recommended cell *also* trails HODL
  (−18%/yr USD vs −13%/yr HODL, rational exercise, measured haircut) —
  the replay's winners hold premium in USD and/or face non-rational
  exercisers. Under measured auction haircuts and rational counterparties,
  **this strategy's USD edge over HODL is negative in most futures**; its
  honest pitch is yield-in-kind (64 bps of SUI per round at Δ = 0.20, ~9%
  of rounds called away) for holders who would hold SUI regardless. The
  frontend copy must not imply USD outperformance.
- Stress (1,000 paths, steady inflows, 1%/round withdrawals, an **80%
  bank-run at round 26**, pessimistic premiums): every queue cleared at its
  locked pps on every path — the two-step withdrawal design holds; the
  bank-run cost is conversion drag, not insolvency.

## 5. Standing follow-ups

- **Forward shadow test** (doc 06 §9.4 milestone 4): once the keeper runs on
  testnet, compare 4+ weeks of its per-round σ/K\*/strike/premium logs to the
  engine's prediction; recalibrate `n_bidders` / `haircut_bps`, then revisit
  this memo before mainnet.
- wBTC-C: rerun this memo's sweeps with `BTC_LADDER` + BTC calibration
  (vrp 1.19 measured directly); expect the same Δ = 0.20-over-0.10 logic to
  hold with shallower haircuts (deeper external markets to arb against).
