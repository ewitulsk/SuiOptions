# Validation report (doc 11 — P5, SO-457)

Status: **FIRST PASS 2026-09-02.** Produced by `desk-backtester` (PR O:
attribution, sweeps, stress suite, walk-forward runner) on the SO-451 exact
ledger, against the lake mirror. Every number below is a conditional
simulation (doc 08 §0.2): `proxy_oracle`, `proxy_venue` (Binance spot/perp
as the path and the mark), `taker_only` unless labeled, `no_resale`,
generated flow from **stated priors** (doc 08 §3.1: no RFQ history exists),
`exercise=american_sweep` on a modeled route with **assumed** flash
capacity, latencies assumed. Nothing here is an APY claim; §12 at the end
says which "definition of validated" items pass.

Machine-readable: `11-results/results.json` + `11-results/report.md`
(the generated doc 08 §9.6 report with the §12 checklist evaluated by the
assembler) plus the stage CSVs, from `desk-backtester report --dir
<study>`; the study is reproduced by `crates/backtester/studies/run_doc11.sh`.

## 0. TL;DR

0. **Under the Bluefin margin rules the doc 07 / doc 10 call desk is
   dead.** The 3× NAV, 30-day ATM call book that doc 10 §2 reported at
   +59 % for Aug 2025 – Jul 2026 ends the same year at **NAV 27 k of
   1 M** with five liquidations once the hedge is an isolated-margin
   10× perp under the doc 08 §0.4 10 %-of-NAV/24 h top-up cap (§2). The
   option gains that would fund the margin are trapped on Sui (doc 08
   §7.3): on the 2026-01-03 → 01-06 rally (+18 %) the short hedge loses
   more than the vault's free cash and is liquidated, and the desk then
   holds naked calls into the next leg down. Nothing in the rest of this
   document changes that ordering: margin survivability, not the vol
   estimator, is the binding question on SUI.
1. **Doc 07 §5 reproduces under doc 07's own assumption** (no margin
   model, `margin.enabled = false`): band-20 turnover 10.8×/30 d against
   doc 07's 11.3× and doc 10's 11.8×; every band within the stated
   tolerance (35 % of doc 07, 25 % of doc 10 — the bar-path fills,
   contract rounding and the reduce-only exercise close of PRs L/M take
   10–25 % off mid-band turnover). Costs scale with turnover exactly as
   doc 07 §5 says. The gate test runs against the mirror
   (`DESK_LAKE_MIRROR=… cargo test --release -p desk-backtester doc07`).
2. **Walk-forward, SUI (doc 07 framing, 2× NAV per turn): no estimator
   clears the policy.** Selection on the two training folds (2024-05 →
   2025-04) picks `har q_bid=0.25`, but every candidate breaches the
   15 % training drawdown gate (`gate_failed_all`), and on the
   validation fold (2025-05 → 2025-10, the cascade in-sample) the
   selected candidate returns **−4 % annualized** (windows −31 %,
   q0.35 −19 %, q0.45 −28 %). The holdout (2025-11 → 2026-07) is
   **sealed**.
3. **Walk-forward, BTC: the HAR forecaster clears the hurdle out of
   sample, once.** Selected on 2021–2023 training folds (`har q_bid=0.25`,
   mean +98 % annualized, but every candidate breaches the 15 % training
   drawdown gate in at least one year), it returns **+85 % annualized on
   the 2024 validation year with a 12 % drawdown** against windows' +25 %
   at 30 %; all three HAR quantiles clear the hurdle on 2024 (+46 % to
   +85 %) at 12–13 % drawdown. One validation fold, one asset, no margin
   model, a 2021 training year whose +205 % is mostly the short hedge
   receiving 1.56 M of funding: doc 10 §4.2's BTC result survives the
   protocol but is not yet a distribution. The holdout (2025-01 →
   2026-07) is **sealed**.
4. **The stress suite is a leverage question.** On the half-year SUI
   mixed book (generated flow, HAR q0.35, doc 08 §0.4 caps) every one of
   the 17 cases stays inside the 15 %/25 % drawdown limits, but at 10×
   hedge leverage 14 of 17 — including the untouched historical replay
   — liquidate at least once. At **3× leverage** the historical replay
   passes (zero liquidations, closest headroom +108 % of MMR) and 14/17
   cases pass; the −60 % gap, the +15 %/day rally with a delayed oracle,
   and the venue outage during exercise still liquidate once each.
5. **Attribution reconciles to the exact ledger everywhere**
   (|Σ lines − ΔNAV| < 1e-7 on every run) and the Greek explanation
   carries a bounded residual (6 % of gross on the −82 % SUI year at an
   hourly step; the largest single hour is the 2025-10-10 cascade).
   Model edge at entry is reported as its own non-realized line on every
   run and never enters a return figure.

## 1. Setup and runtime

| stage | scenario | window | runs | wall time (8 cores) |
|---|---|---|---:|---:|
| doc 07 reproduction | `sui_doc07_calls.toml`, bands 1.5–30, margin off + on | 2025-08-01 → 2026-07-31 | 9 | 4 min |
| SUI walk-forward | `studies/wf_sui_estimator.toml` (4 candidates) | folds 2024-05 → 2025-10, holdout sealed | 12 | 8 min |
| BTC walk-forward | `studies/wf_btc_estimator.toml` (4 candidates) | folds 2021 → 2024, holdout sealed | 16 | 25 min |
| stress suite ×2 | `sui_mixed_halfyear.toml`, at 2025-10-10, leverage 10 and 3 | 2025-08-01 → 2026-01-31 | 34 | 6 min |
| break-even grid | `studies/grid_sui_halfyear.toml`: band × execution × leverage × mix, 2 seeds | half-year | 48 | 10 min |
| capacity frontier | `sui_mixed_halfyear.toml`, V = 25 k per day, balanced, 2 seeds (cut from two volumes: the second point would have pushed the serial bisection past 45 min) | half-year | 28 + 6 | 12 min |

The expensive stages run on the **half-year SUI window** (2025-08-01 →
2026-01-31, the 2025-10-10 cascade in-sample) on purpose: the mixed-book
scenario revalues every 15 minutes with hundreds of open positions and a
full year costs three times as much for the same regimes. Data: gold 60 s
bars binance SUI-USDT (coverage 99.98 %) and BTC-USDT-PERP, silver
funding_rates (Binance settlements), no vol index (the BTC DVOL leg is doc
10 §4). Determinism hashes are in every `summary.json`.

## 2. Doc 07 §5 / doc 10 §2 reproduction (SUI calls, Aug 2025 – Jul 2026)

Desk as doc 10 §1: one 30-day ATM call per turn at 3× NAV of spot notional,
fair − 5 vol points, 30 % premium cap in one expiry, taker hedge at 3.5 bp +
3.5 bp slippage + 0.03 flat, funding at every settlement, the PR M
exercise route. Two framings: **doc 07's** (no margin model) and the
**Bluefin isolated-margin** model of PR L (IMR 4.5 %, MMR 2.5 %, 10×, top-up
trigger 3.5 %, 10 % NAV/24 h cap).

### 2.1 Margin off (`margin_model = none(doc07_reproduction)`)

| band %NAV | turnover ×NAV/30d | doc 07 | doc 10 §2 | vs doc 07 | vs doc 10 | fees+slip %NAV/30d | doc 07 @3.5bp | year-end NAV | max DD | fills |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1.5 | 50.2 | 76.4 | 60.2 | −34 % | −17 % | 3.57 | 2.67 | 938,214 | 0.484 | 8 |
| 3 | 34.3 | 48.5 | 42.6 | −29 % | −19 % | 2.44 | 1.70 | 1,104,630 | 0.419 | 8 |
| 5 | 24.6 | 33.1 | 31.7 | −26 % | −23 % | 1.74 | 1.16 | 1,184,451 | 0.386 | 8 |
| 10 | 14.9 | 19.1 | 19.1 | −22 % | −22 % | 1.06 | 0.67 | 1,308,386 | 0.380 | 8 |
| 20 | 10.8 | 11.3 | 11.8 | −4 % | −8 % | 0.77 | 0.39 | 1,586,596 | 0.317 | 8 |
| 30 | 6.0 | 8.3 | 7.2 | −27 % | −16 % | 0.43 | 0.29 | 1,190,081 | 0.357 | 8 |

Turnover is within tolerance at every band. It sits 10–25 % under doc 10
§2 at the tight bands for three reasons that are all engine fidelity, not
regressions: fills execute on the bar path with 1-SUI contract rounding
(PR L), the exercise sweep closes the hedge reduce-only as slices are
detected instead of re-hedging the whole book at expiry (PR M and this
PR), and 8 turns filled instead of doc 10's 9 (five turns are declined
because a 30-day call at the post-cascade sigma exceeds the 30 % cap —
the v0 constant injector consumes a declined turn). The cost column
reproduces doc 07's shape (∝ turnover) with the flat fee and slippage doc
07's table omits, as doc 10 §2 already noted.

The band-20 year, attributed (doc 08 §9.1, `attribution.json`):

| line | value (of 1 M NAV) |
|---|---:|
| NAV 1,000,000 → 1,586,596; exact return +58.7 %, +53 % after idle-cash cost at the 4 % settlement yield | |
| premium paid | −2,468,826 |
| option payoff (cash realized by exercise) | +906,380 |
| option leg (payoff − premium) | **−1,562,446** |
| of which model edge at entry (fair − bid; **non-realized**) | +241,117 |
| of which mark-to-market while held | −1,526,880 |
| of which exit vs last mark (route cost, expiry to zero) | −155,444 |
| perp realized + unrealized | **+2,016,594** |
| funding (short receives) | +57,396 |
| taker fees / slippage / fixed | −46,187 / −46,171 / −16 |
| exercise cost (intrinsic at the decision price − net received) | 64,866 |
| Greek explanation of the marks: delta +188 k, gamma +1,220 k, theta −1,448 k, vega +231 k, basis 0, residual +345 k (6 % of gross) | |

By regime (30-day trailing return × vol tier): the whole return is the
`crash/high_vol` stretch (92 days, +59 %: gamma +196 k, vega +229 k against
theta −207 k on the hedge's +1.1 M); `range/low_vol` (158 days) is −7 %
with theta −916 k against gamma +737 k. The option leg lost in every
regime; the year is the short hedge's, exactly doc 10 §0.4's warning.

### 2.2 Margin on (`margin_model = isolated(bluefin_rules)`, 10×)

| band %NAV | year-end NAV | max DD | liquidations | forfeited margin | turnover ×NAV/30d | fills |
|---:|---:|---:|---:|---:|---:|---:|
| 5 | 15,593 | 0.992 | 5 | 176 k | 8.5 | 8 |
| 20 | 26,958 | 0.987 | 5 | 177 k | 7.0 | 8 |
| 30 | 151,369 | 0.913 | 2 | — | 4.7 | 8 |

Same path, same fills, same estimator. A 3× NAV call book hedges with a
≈ 1.5× NAV short perp; at 10× that is 15 % of NAV of entry margin, a 10 %
rally wipes it, and the 24 h top-up cap (10 % of NAV) cannot refill it
before the next leg. The vault's option marks rise by more than the perp
loses, but they are on Sui and the venue does not see them. After the
liquidation the desk holds naked calls; the remaining cash goes to the
next entry margin; the second liquidation takes that. Two engine fixes
this PR made while measuring it: the exercise hedge-close is now
**reduce-only** (it could open an unfunded perp after a liquidation and
cascade through hundreds of zero-margin fills), and a run halts when NAV
≤ 0 (`bankrupt_ms`) instead of trading negative cash.

## 3. Attribution (doc 08 §9.1) — what the layer reports and how it closes

Every run now writes `attribution.json`: cumulative, per option type, per
regime, per turn (fill-to-fill under per-turn flow, calendar month
otherwise), with these lines: exact realized cash return and CAGR of exact
NAV, the CAGR after the idle-cash opportunity cost, model edge at entry
(explicitly non-realized), option payoff/resale, mark-to-market and
exit-vs-mark, perp realized/unrealized, funding by long/short direction,
maker/taker fees, slippage, gas, fixed costs, exercise cost (intrinsic at
the decision price minus net received) and the PTB path counts,
liquidation loss and closest margin headroom, and the
delta/gamma/theta/vega/basis explanation with its residual.

The totals are the SO-451 ledger's own lines: `spread` (entry edge),
`option_mark`, `option_exit`; the layer only splits them by type and Greek
and asserts the split sums back (`option_identity_gap`,
`perp_identity_gap` ≈ 1e-9 on every run). Windows reconcile through the
ledger's `nav_explained` identity — `reconciliation_gap` is < 1e-7 on all
~140 runs of this study. Capacity runs also carry depositor-net profit per
accepted Earn notional and return on peak capital deployed.

Hand checks (`gates.rs`): a one-day put and a call+put pair on a flat path
reproduce the Black-Scholes premium at the lattice strike minus the V1
bid's expected hedge cost (7 bp + 0.03 + the long hedge's expected
funding) to 2 %, the long hedge's units, fees, slippage and funding to the
cent, and the mixed pair nets its delta inside the band and never hedges;
put-heavy and call-heavy variants hedge in opposite directions.

## 4. Walk-forward estimator study (doc 08 §9.2)

Protocol (`walkforward.rs`): chronological folds from config; every
candidate runs on the training folds; the candidate with the best mean
depositor-net annualized return among those with **zero liquidations,
no bankruptcy and training drawdown ≤ 15 %** is selected; validation is
then run for every candidate and reported, never re-ranked; the holdout
runs only with `--open-holdout`, only for the selected candidate, and the
manifest records `ranked_on = ["train"]`, every run's window and the data
each fold could read (`data_from`: the longest warm-up any candidate needs
— a year for HAR — clamped to `calibration_from`). A trace test
(`selection_reads_only_past_folds_and_the_holdout_stays_sealed`) proves
the selection happens after only training runs, that every training fold
ends before validation begins, and that the holdout is not run without the
flag and does not change the selection when it is.

Candidates: `windows` (the live two-window blend, rp 0.05, lean 0.8) and
`har` at `q_bid` ∈ {0.25, 0.35, 0.45} (rp 0, lean 0). Both studies use doc
07's framing (no margin model, §2.2 answers that question) so the
estimator is what differs.

### 4.1 SUI (`wf_sui_estimator.toml`, 2× NAV per turn, 30-day ATM calls)

Folds: train-1 2024-05-01 → 10-31, train-2 2024-11-01 → 2025-04-30,
validation 2025-05-01 → 10-31, holdout 2025-11-01 → 2026-07-31 (sealed).

| candidate | eligible | train-1 | train-2 | train mean | validation | val max DD | σ paid / realized (val) |
|---|---|---:|---:|---:|---:|---:|---|
| windows | no (train DD 0.249) | +8.5 % | +67 % | +38 % | **−31 %** | 0.378 | 0.92 / 1.01 |
| har q0.25 | no (train DD 0.245) | +43 % | +113 % | +78 % | **−4 %** | 0.215 | 0.85 / 1.01 |
| har q0.35 | no (train DD 0.252) | +16 % | +112 % | +64 % | −19 % | 0.238 | 0.87 / 1.04 |
| har q0.45 | no (train DD 0.279) | −11 % | +63 % | +26 % | −28 % | 0.310 | 0.95 / 1.01 |

Selected (score only, `gate_failed_all = true`): **har q0.25**. Reading:
the two training folds are the 2024-H2 rally and the 2025-H1 slide, and
the ordering flips between them (windows +8 %/+67 %, q0.25 +43 %/+113 %);
the validation fold is the cascade. Nothing on SUI clears the 15 %
drawdown policy on any fold, and the selected candidate is below the 12 %
hurdle on validation. With one validation fold there is no confidence
interval (n = 1); doc 08 §12 items 7 and 8 do not pass on SUI. The
forecaster's lower bids (σ paid 0.85 vs windows 0.92 on validation) cost
volume, not money — doc 10 §4.2's reading holds out of sample — but "less
bad" is the whole result.

### 4.2 BTC (`wf_btc_estimator.toml`, 3× NAV per turn, 30-day ATM calls, 5-minute sampling)

Folds: train 2021, 2022, 2023 (each with data from a year before);
validation 2024; holdout 2025-01-01 → 2026-07-31 (sealed).

| candidate | eligible | 2021 | 2022 | 2023 | train mean | **2024 validation** | val max DD | val CVaR-95 (daily) | σ paid / realized (val) |
|---|---|---:|---:|---:|---:|---:|---:|---:|---|
| windows | no (2021 DD 0.325) | +22 % | +16 % | +13 % | +17 % | +25 % | 0.302 | −5.5 % | 0.44 / 0.53 |
| har q0.25 | no (2021 DD 0.353) | +205 % | +19 % | +69 % | +98 % | **+85 %** | **0.123** | −1.0 % | 0.36 / 0.53 |
| har q0.35 | no (2021 DD 0.244) | +224 % | +9 % | +45 % | +92 % | +64 % | 0.123 | −1.1 % | 0.40 / 0.53 |
| har q0.45 | no (2021 DD 0.297) | +164 % | −2 % | +36 % | +66 % | +46 % | 0.134 | −1.0 % | 0.44 / 0.53 |

Selected (score only, `gate_failed_all = true`): **har q0.25**. Reading:

- The forecaster beats the blend in every fold and on validation, by
  bidding lower (σ paid 0.36 against the blend's 0.44 on 2024, realized
  0.53) and turning over more (20× vs 13× NAV/30 d — the lower bid buys
  more units for the same 30 % budget). Its validation drawdown (12 %) is
  inside the policy; the blend's (30 %) is not.
- 2021 is the outlier that decides the training score: +205 % for q0.25,
  of which **1.56 M of funding received** by the short hedge on a 1 M
  book (2021 perp funding) and the option leg +3.1 M against the hedge
  −2.4 M. 2022 (−65 % BTC) is flat for every candidate (option −0.7 M,
  hedge +0.8 M, funding −0.09 M): the book is direction-neutral and the
  vol was priced about right (bias +0.15). 2023 is the gamma year.
- Every candidate breaches the 15 % drawdown gate in 2021, so the
  selection is by score with the gate flagged; on the validation year the
  HAR candidates' 12–13 % drawdown is the first time in this study a
  candidate sits inside the policy on an out-of-sample fold.
- No confidence interval: one validation fold. Doc 08 §12 item 8 stays
  failed on BTC until the fold count grows (the holdout, when opened,
  adds one more point, not an interval).

## 5. Synthetic stress suite (doc 08 §9.5)

Base: `sui_mixed_halfyear.toml` — generated market-mode flow at the stated
priors with the acceptance hazard, HAR q0.35 bids, 14-day board expiries,
doc 08 §0.4 caps, Bluefin margin, the modeled exercise route. Stress
instant 2025-10-10; the outage case straddles the 2025-10-16 expiry.
Limits: 15 % drawdown on the historical replay, 25 % on a stress, zero
liquidations. Every case is a transformation of the real path or an
override, run through the same engine (`stress.rs`).

| case | transformation | 10× hedge: NAV end / DD / liq | 3× hedge: NAV end / DD / liq / headroom |
|---|---|---|---|
| historical | none (15 % limit) | 1,062,563 / 0.051 / **1** | 1,064,714 / 0.051 / 0 / +1.08 ✓ |
| gap_down_60 | price × 0.40 at 10-10 | 1,061,957 / 0.131 / **5** | 1,172,003 / 0.064 / **1** / −3.5 |
| gap_up_80 | price × 1.80 | 1,350,805 / 0.054 / **1** | 1,324,267 / 0.051 / 0 ✓ |
| crash_multistep_delayed_oracle | −12 %/day × 5, oracle every 5 min at 60 s | 1,190,234 / 0.059 / **1** | 1,191,720 / 0.059 / 0 ✓ |
| rally_multistep_delayed_oracle | +15 %/day × 5, same oracle | 1,049,276 / 0.095 / **4** | 1,070,466 / 0.084 / **1** / −3.5 |
| flat_six_months | price pinned ±0.1 % 183 d, funding 0 | 964,999 / 0.051 / 0 ✓ | 965,000 / 0.051 / 0 ✓ |
| vol_collapse_after_purchase | returns × 0.25 from 10-11 | 1,012,669 / 0.051 / 0 ✓ | 1,012,670 / 0.051 / 0 ✓ |
| funding_plus_50 | +50 %/yr × 30 d | 1,055,092 / 0.062 / **3** | 1,061,022 / 0.062 / 0 ✓ |
| funding_minus_50 | −50 %/yr × 30 d | 1,073,924 / 0.065 / **1** | 1,076,086 / 0.065 / 0 ✓ |
| venue_outage_exercise_margin | outage expiry−12 h → +36 h, price × 0.75 at its start | 1,128,843 / 0.074 / **4** | 1,161,803 / 0.074 / **1** / −0.05 (23 top-ups declined, 10 rejected) |
| sui_congestion_near_expiry | inclusion 10 ± 5 min, detection 2 min, 20 % PTB failure, whole run | 1,076,928 / 0.051 / **1** | 1,084,718 / 0.051 / 0 ✓ |
| no_resale | resale off (already the base) | = historical | = historical ✓ |
| no_base_flash | pool base 0 | = historical | = historical ✓ |
| no_quote_flash | pool quote 0 | = historical | = historical ✓ |
| router_depth_collapse | route depth ÷ 20 | 1,066,161 / 0.051 / 0 ✓ (exercise cost 2.8 k → 10.9 k) | same ✓ |
| concentrated_expiry | every writer herds into the nearest expiry, per-expiry cap lifted | 997,498 / 0.067 / **2** | 999,441 / 0.067 / 0 ✓ |
| settlement_depeg | −300 bp mark basis for 7 d | 1,065,288 / 0.051 / **2** | 1,067,664 / 0.051 / 0 ✓ |

Reading:

- **Drawdown is never the binding limit** on this book: 5–13 % against
  15/25 %. Liquidation is. At 10× the untouched replay liquidates once
  (closest headroom −3 % of MMR, 11.9 k forfeited) and 14/17 cases
  liquidate; at 3× the replay passes with +108 % headroom and 14/17
  cases pass. The three that still fail at 3× are the ones that move the
  mark faster than a 24 h top-up cap can follow (−60 % instantly, +15 %/day
  for five days) or refuse the top-up outright (the outage).
- The flash-liquidity cases coincide with the replay because the mixed
  book's puts exercised through the vault-cash paths in this window and
  the base never resells; they are labeled as such rather than claimed as
  passes of something that was not exercised.
- Router depth collapse quadruples the exercise cost and changes nothing
  else; the flat six months lose the option theta (−3.5 %) with zero
  liquidations either way — the survivable no-move case.
- The gap up (+80 %) makes money at both leverages (calls) and still
  liquidates once at 10×: the rally liquidation is the trapped-gain
  mechanism of §2.2 in miniature.

## 6. Break-even surface (doc 08 §9.3/§9.4)

`studies/grid_sui_halfyear.toml` on the half-year mixed book: hedge band
{10, 20} % NAV × execution assumption {taker_only (central), conservative}
× hedge leverage {3, 10} × mix {balanced, call_heavy 85 %, put_heavy 15 %},
two flow seeds with common random numbers (the same arrivals, sizes,
buckets and acceptance draws at every point; the mixes rescale the
per-type arrival rates at constant total intensity). A point breaks even
when the median depositor-net annualized return clears the 12 % hurdle,
the worst seed's drawdown is ≤ 15 %, and no seed liquidates or goes
bankrupt. **0 of 24 points break even**; liquidation is the binding line at
21 of them and the hurdle at the other 3 (`surface.csv`).

| band | execution | leverage | mix | net median (ann.) | ci95 (n = 2) | after idle cost | worst DD | liq (2 seeds) | fills | accepted | binding | label |
|---:|---|---:|---|---:|---|---:|---:|---:|---:|---:|---|---|
| 10 | taker | 3 | balanced | +12.4 % | [−26 %, +51 %] | +8.5 % | 0.099 | 2 | 2,944 | 12.5 M | liquidation | demand_limited |
| 10 | taker | 3 | call_heavy | +5.0 % | [−76 %, +86 %] | +1.1 % | 0.050 | 1 | 2,586 | 10.1 M | liquidation | uneconomic |
| 10 | taker | 3 | put_heavy | +17.4 % | [−5 %, +40 %] | +13.5 % | 0.186 | 2 | 3,057 | 14.1 M | liquidation | capital_limited |
| 10 | taker | 10 | balanced | +6.3 % | [−48 %, +61 %] | +2.4 % | 0.108 | 11 | 2,945 | 12.5 M | liquidation | uneconomic |
| 10 | taker | 10 | call_heavy | +5.0 % | [−44 %, +54 %] | +1.1 % | 0.050 | 2 | 2,586 | 10.1 M | liquidation | uneconomic |
| 10 | taker | 10 | put_heavy | +20.7 % | [+1 %, +41 %] | +16.5 % | 0.150 | 10 | 3,067 | 14.2 M | liquidation | demand_limited |
| 10 | conservative | 3 | balanced | −6.4 % | [−19 %, +6 %] | −9.7 % | 0.185 | 4 | 2,942 | 12.5 M | liquidation | uneconomic |
| 10 | conservative | 3 | call_heavy | +3.3 % | [−72 %, +78 %] | −0.5 % | 0.049 | 1 | 2,584 | 10.1 M | liquidation | uneconomic |
| 10 | conservative | 3 | put_heavy | +18.6 % | [−65 %, +102 %] | +14.7 % | 0.242 | 2 | 3,053 | 14.1 M | liquidation | capital_limited |
| 10 | conservative | 10 | balanced | −3.6 % | [−33 %, +26 %] | −7.1 % | 0.163 | 9 | 2,942 | 12.5 M | liquidation | uneconomic |
| 10 | conservative | 10 | call_heavy | +2.9 % | [−77 %, +83 %] | −1.0 % | 0.055 | 2 | 2,584 | 10.1 M | liquidation | uneconomic |
| 10 | conservative | 10 | put_heavy | −6.2 % | [−59 %, +46 %] | −9.6 % | 0.242 | 10 | 3,018 | 14.0 M | liquidation | uneconomic |
| 20 | taker | 3 | balanced | +14.2 % | [−97 %, +126 %] | +10.2 % | 0.072 | 1 | 2,927 | 12.5 M | liquidation | demand_limited |
| 20 | taker | 3 | call_heavy | +5.2 % | [−32 %, +42 %] | +1.3 % | 0.064 | **0** | 2,573 | 10.1 M | median < hurdle | uneconomic |
| 20 | taker | 3 | put_heavy | +16.0 % | [−132 %, +164 %] | +12.2 % | 0.196 | 3 | 3,045 | 14.1 M | liquidation | demand_limited |
| 20 | taker | 10 | balanced | +19.6 % | [+11 %, +28 %] | +15.3 % | 0.061 | 5 | 2,926 | 12.5 M | liquidation | demand_limited |
| 20 | taker | 10 | call_heavy | +5.2 % | [−31 %, +42 %] | +1.3 % | 0.064 | 1 | 2,573 | 10.1 M | liquidation | uneconomic |
| 20 | taker | 10 | put_heavy | +13.3 % | [−33 %, +60 %] | +9.3 % | 0.163 | 11 | 3,055 | 14.1 M | liquidation | demand_limited |
| 20 | conservative | 3 | balanced | +8.2 % | [−37 %, +54 %] | +4.3 % | 0.085 | 1 | 2,925 | 12.5 M | liquidation | uneconomic |
| 20 | conservative | 3 | call_heavy | +4.8 % | [−30 %, +39 %] | +1.0 % | 0.048 | **0** | 2,571 | 10.1 M | median < hurdle | uneconomic |
| 20 | conservative | 3 | put_heavy | +7.7 % | [−171 %, +186 %] | +4.2 % | 0.190 | 3 | 3,016 | 14.0 M | liquidation | uneconomic |
| 20 | conservative | 10 | balanced | +12.0 % | [−36 %, +60 %] | +8.0 % | 0.064 | 4 | 2,926 | 12.5 M | liquidation | demand_limited |
| 20 | conservative | 10 | call_heavy | +4.8 % | [−30 %, +39 %] | +0.9 % | 0.048 | **0** | 2,571 | 10.1 M | median < hurdle | uneconomic |
| 20 | conservative | 10 | put_heavy | −8.2 % | [−20 %, +3 %] | −11.5 % | 0.212 | 8 | 3,002 | 13.9 M | liquidation | uneconomic |

Sensitivity (other axes at their base value — band 10, taker, 3×,
balanced):

| axis | values | median net | range |
|---|---|---|---:|
| hedge band | 10 → 20 % NAV | +12.4 % → +14.2 % | 1.8 pts |
| execution assumption | taker (central) → conservative | +12.4 % → −6.4 % | **18.8 pts** |
| hedge leverage | 3× → 10× | +12.4 % → +6.3 % | 6.1 pts |
| mix | balanced / call-heavy / put-heavy | +12.4 % / +5.0 % / +17.4 % | 12.4 pts |

Reading:

- **Nothing on this window survives the zero-liquidation policy at any
  leverage** with the doc 08 §0.4 top-up cap; the only liquidation-free
  points are the call-heavy books at band 20, and they earn +5 %. The
  put-heavy books earn the most (+13 to +21 % median) and liquidate the
  most (long hedge into the October cascade).
- The two-seed intervals are enormous (n = 2, Student-t 12.7 × sd/√2):
  the seeds differ by which side of the cascade the book's herded expiry
  sits on. This is the honest width of a two-seed study; doc 08 §9.6's
  distributions need eight seeds and the runtime that implies.
- The execution assumption is the largest single sensitivity: the
  `conservative` passive assumption (orders must be traded through, else
  they time out and escalate to takers) costs 10× the fee line (122 k vs
  11 k on the balanced 10 %/3× point) and flips the sign. Central and
  conservative are both published, as the P5 gate asks; neither is
  calibrated (proxy-BBO era, doc 08 §7.2).
- Band width barely matters on the mixed book (1.8 pts), the doc 07 §5
  saturation shape again; leverage matters through the liquidation count
  (2 at 3× vs 11 at 10× on the balanced point) more than through the
  median.
- Depositor-net profit per accepted notional at the best liquidation-free
  point (band 20, taker, 3×, call-heavy): +0.3 % of accepted notional over
  six months on 10.1 M accepted; return on peak capital deployed is in
  every `summary.json`.

## 7. Capacity frontier (doc 08 §8.6)

One point, not a frontier: the solver was cut to **V = 25 k/day accepted
spot notional, balanced mix, two flow seeds** on the half-year window
after the second volume (100 k/day) would have pushed the serial
bisection past 45 minutes; the 100 k point and the log sweep of doc 08
§8.1 are the first thing to run when the study is repeated with time.
Capacity mode injects the target inelastically (`acceptance = instant`),
so a point is never `demand_limited`.

| target accepted/day | mix | min NAV (95 % of seeds) | per seed | binding (simulated) | next | §8.6 lower bound | agrees | net (ann.) at min NAV | hurdle | max DD | liq | label |
|---:|---|---:|---|---|---|---:|---|---:|---|---:|---:|---|
| 25,000 | balanced | **743,180** | 649 k / 743 k | **liquidation** | premium_per_expiry, drawdown | 506,459 (peak-expiry premium) | no | +15.1 % | 2/2 pass | 0.066 | 0 | capital_limited |

At the solved NAV (28 bisection runs + 6 probes): accepted 4.61 M over
the half year (4,412 RFQs, 2,207 calls / 2,206 puts, 4,066 expiries),
premium turnover 131 k, hedge turnover 287 k, exercise spot turnover
1.55 M (556 calls / 978 puts exercised, none laddered or failed); peak
premium at risk 75 k total (call 51 k, put 74 k, one expiry 51 k);
initial hedge margin 16 k, peak 24 h top-up 28 k (38 % of the daily
release fraction), external budget usage 19 %; displayed writer-net APY
106 % calls / 104 % puts; depositor-net **+15.1 % annualized, 6.6 %
drawdown, zero liquidations at both seeds**. Net profit per accepted
notional ≈ +1.2 % over six months; return on peak capital deployed
(80 k of marks + reservations + margin) is large because the solved NAV
is ten times the capital the book ever ties up.

Reading: the binding constraint is **liquidation**, not premium — just
below 650–740 k of NAV the 10× hedge liquidates on the cascade, while the
doc 08 §8.6 lower bound (which has no liquidation term; its
`synthetic_stress_loss` is still zero) says 506 k on the peak-expiry cap.
The bound and the simulator disagree (`lower_bound_agrees = false`), which
is the §8.6 "necessary, not sufficient" case made concrete: the required
NAV is set by how much cash must sit idle to survive a margin call, not by
the premium caps. Every column doc 08 §8.6 asks for is in
`capacity/capacity-V25000-balanced/summary.json` and `frontier.csv`.

## 8. Labels carried by every published result

`proxy_oracle=true`, `proxy_venue=true`, `execution=taker_only |
conservative`, `resale=no_resale`, `flow=constant | generated_market |
capacity_injection(demand_inelastic)`, `acceptance=instant | hazard_ttl`,
`flow_provenance=prior (stated, uncalibrated)`, `estimator=windows |
har(q_bid=…)`, `exercise=american_sweep`, `margin_model=isolated
(bluefin_rules) | none(doc07_reproduction)`, `flash_capacity_assumed=true`,
`venue_capacity=assumed`, `gap_policy=invalidate`, `latency_assumed=true`,
`sui_inclusion_ms=…`, `basis_configured=…`, plus `bankrupt=true` where a
run died. Coverage and invalidated spans are on every `Metric`;
distributions carry n, sd, quantiles, a Student-t interval and CVaR.

## 9. Doc 08 §12 — definition of validated

| # | item | status | why |
|---:|---|---|---|
| 1 | Exact ledger reconciliation passes every event and full replay | **pass** | SO-451 asserts every event; this PR's windows close through `nav_explained` on every run (worst gap < 1e-7 of NAV) |
| 2 | Live and simulation adapters produce identical commands for identical traces | not testable here | the kernel smoke drives `DeskKernel` from the backtester; no recorded live trace exists yet |
| 3 | The strategy cannot create written options | by construction | the engine has no write path |
| 4 | Calls and puts quote, reserve, hedge, resell, expire, exercise | pass | engine / solver / gates tests (§3) |
| 5 | All three put PTBs and their fallback pass atomic failure tests | pass | PR M tests, re-run here |
| 6 | No-resale mode completes and is economically survivable | **pass at 3×, fail at 10×** | §5 `no_resale`: drawdown 5 %, zero liquidations at 3× |
| 7 | Results clear the hurdle on the untouched holdout | **sealed** | holdouts not opened; SUI validation is −4 % anyway |
| 8 | The lower confidence bound clears the hurdle | **fail** | one validation fold (n = 1, no interval) on SUI; grid points: see §6 |
| 9 | Historical and synthetic stresses inside drawdown and liquidation limits | **fail** | drawdowns pass everywhere; liquidations fail 14/17 at 10× and 3/17 at 3× |
| 10 | Margin top-ups feasible without violating premium/liquidity constraints | **pass at 3×, fail at 10×** | historical replay: 0 declines / 0 rejects / 0 liquidations at 3× |
| 11 | Acceptable across call-heavy, put-heavy and mixed flow | **fail** | §6: 0/24 grid points break even; call-heavy is liquidation-free but below the hurdle, put-heavy clears the hurdle and liquidates |
| 12 | Profit does not depend on one latency, queue, IV, resale or seed assumption | **fail** | §4: the estimator ordering flips between folds; §6 sensitivity |
| 13 | Capacity bounded by measured depth, flash balances, router depth, expiry concentration | **fail** | every capacity result is `venue_capacity=assumed` / `flash_capacity_assumed` (doc 08 §10) |
| 14 | Every target volume has min-NAV, CI, binding constraint, feasibility label | **pass (one volume)** | §7: min NAV, per-seed interval, simulated binding + next two, §8.6 bound, `capital_limited`; only V = 25 k/day was run |
| 15 | Model edge is never presented as realized revenue | by construction | one line, `model_edge_at_entry_non_realized`, outside every return figure |
| 16 | Every published result includes uncertainty, coverage and proxy labels | pass | §8 |

The strategy is **not validated**. What this pass established is the
failure surface doc 08 §12 says to report until it is: the call desk's
survivability is a hedge-leverage and top-up-policy question before it is
an estimator question; on SUI no estimator clears the drawdown policy on
any fold; and every capacity statement rests on assumed venue depth.

## 10. Caveats specific to this pass

- One validation fold per asset: the walk-forward reports a point, not a
  distribution; more folds need more history (SUI has 3.3 years).
- The half-year mixed-book window contains one regime (the cascade and
  its aftermath); the grid and stress numbers are that regime's.
- `sui_congestion_near_expiry` applies the congestion to the whole run (a
  conservative superset); `settlement_depeg` is a mark-basis proxy.
- Market-mode arrivals at the stated priors accept ~11–15 M of notional in
  six months on a 1 M vault; that is the prior, not demand.
- The constant injector still consumes a declined turn (doc 10 §6).
- The capacity stage is one volume (25 k/day); the 100 k point and the
  log sweep were cut for runtime (§7).
