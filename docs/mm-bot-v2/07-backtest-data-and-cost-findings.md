# Backtest data inventory & execution-cost findings

Status: **RESEARCH FINDINGS, 2026-08-15.** Nothing here is implemented.
This records what the data room can and cannot answer about the V1
strategy, and — more importantly — three measured results that change
parameters already written into `00-plan.md` and `hedge.rs`. Companion
docs: `00-plan.md` (strategy spec), `01-perps-venues.md` (venue
findings), `06-dbm-removal.md` (why Bluefin), `TODO.md`.

The strategy under examination is V1 as clarified 2026-08-15: the bot
buys covered calls from retail on the Earn page, lists them on the
exchange orderbook for resale, and delta-hedges the inventory with perps
while it holds it. SUI is the launch underlying. The RFQ API is gated so
only makers may respond; all other flow arrives through our own frontend.

---

## 0. TL;DR — the three findings that matter

1. **The ±1.5% NAV delta band in `00-plan.md` is wrong by an order of
   magnitude.** Risk reduction saturates around 10–20% NAV bands. Going
   from 20% to 1.5% costs 6.8× the turnover and buys 0.8pp of P&L
   standard deviation. This single parameter dominates every venue,
   fee and execution-mode decision below (§5).
2. **We are not starting with zero SUI data.** Binance Vision has
   SUIUSDT spot trades, perp trades and funding from 2023-05 to
   2026-07, and `vision-sync`/`binance_vision` already handle it with
   no code change. We have zero *implied* vol for SUI; we have 3.2
   years of *realized* vol (§1).
3. **Execution microstructure is a rounding error next to the vol
   estimator.** Hedging, venue fees and flash-loan exercise together
   cost ~0.2–0.8% of NAV per turn against a ~1.7% edge. On the one
   sample tested, pricing at trailing realized vol lost ~2.7% of NAV
   per turn. Bid discipline and the IV estimator are the whole game
   (§10).

---

## 1. Data inventory

### 1.1 What is in the lake today

Bucket `options-data-room-20260813122351104900000001`, verified 2026-08-15.

| Series | Coverage | Granularity |
|---|---|---|
| `silver/trades` BTCUSDC spot | 2018-12-15 → now (2,636d, 4.3 GB) | tick, ~118k/day |
| `silver/trades` BTCUSDT perp | 2019-09-08 → now (2,532d, 68 GB) | tick, ~1.08M/day |
| `silver/funding_rates` binance | 2020-01-01 → 2026-07-31 | 8h settled |
| `silver/funding_rates` hyperliquid | 2023-05-12 → now | 1h settled |
| `silver/vol_index` BTC/ETH DVOL | 2021-03-24 → now (1,971d) | **hourly OHLC** |
| `silver/options_quotes` deribit BTC | **2026-08-14 → now (1 day)** | 60s full-chain snapshots |
| `gold/bars` | full history, both instruments | 1s / 60s / 3600s |
| `gold/rv` | full history | 5 intervals × 3 windows × 2 sources × 2 estimators |
| `gold/gaps` | full history | per-stream, feeds `rv.coverage` |

Two corrections to the standing mental model:

- **Strike-level IV exists going forward.** The Deribit poller
  (`get_book_summary_by_currency`, 60s) landed 2026-08-14 and populates
  `options_quotes` with `mark_iv` on 100% of rows, bid/ask on 87.7%,
  plus `underlying_price` and `open_interest`, across 864 contracts.
  There is no *history* — one day — but it is accumulating now.
- **DVOL gives 5 years of real BTC implied vol.** Hourly, back to
  2021-03. This is the reference series for the ablation in §3.

### 1.2 SUI is one config flag away

Binance Vision coverage, verified by listing the bucket 2026-08-15:

| Dump | Coverage | Files |
|---|---|---|
| SUIUSDT spot trades | 2023-05 → 2026-07 | 39 monthly |
| SUIUSDT perp trades | 2023-05 → 2026-07 | 39 monthly |
| SUIUSDT fundingRate | 2023-05 → 2026-07 | 39 monthly |
| SUIUSDT **bookTicker** (real BBO) | 2023-05 → **2024-04** | 12 monthly |
| SUIUSDC spot trades | 2024-01 → | — |

`vision-sync --symbols/--kinds` are CLI args defaulting to `BTCUSDC`,
and `binance_vision::split_symbol` already resolves `SUIUSDT →
(SUI, USDT)`. Spot, perp and funding need **no code change**.

`bookTicker` needs a `parse_book_ticker_csv` alongside the existing
`parse_trades_csv` / `parse_funding_csv`. It is worth writing: it is the
only real quote data we will ever get for SUI, and it is the only way to
*measure* passive fill rates rather than assume them (§7).

### 1.3 What is missing, and what it blocks

- **No Sui-venue data at all** — no Bluefin, no Aftermath, no DeepBook
  book depth. Every cost number in this document is **fees-only with
  zero slippage**. At scale, depth sets hedge cost, not fee schedules.
  This is the largest single gap.
- **No L2 depth anywhere** (P4, deferred). `book_top` exists for 2 days.
- **`ts_recv` is NULL on all archive rows** — by design. The spec is
  explicit that archive-only backtests must model latency explicitly or
  they manufacture latency-free alpha.
- **No option *trade* data** — Deribit capture is book summaries
  (quotes), so no fill quality and no flow toxicity.
- **Binance funding stops 2026-07-31** — monthly dump publication lag,
  not a bug.
- **`tools/backtester` + `crates/vault-sim` are the wrong engine** —
  daily OHLC CSVs, synthetic paths, targeting the deprecated
  covered-call vault. Daily bars destroy the intraday rebalancing that
  the P&L actually lives in. (Being cut separately.)

---

## 2. What cannot be backtested at all

Our own exchange's flow. There is no history of what retail will RFQ, at
what size, when, or with what adverse selection.

The RFQ gate genuinely helps: with makers restricted to *responding* and
all other flow arriving through our frontend, flow stops being a hostile
unknown and becomes a design variable — we choose the strike grid, the
expiries, and what is displayed. That makes it a swept parameter rather
than an adversary.

Two caveats worth recording:

- **The gate is a speed bump, not a trust boundary.** The frontend talks
  to our backend over a wire anyone can inspect and replay. Fine at
  launch scale; do not set risk limits that assume it holds.
- **Two adverse-selection channels survive it.** (a) Behavioural: retail
  sells calls after run-ups — a price-conditional arrival rate, and the
  realistic flow model to build. (b) American exercise: anyone holding
  our options can exercise at any time. Harmless while the bot is long;
  live the moment it writes.

---

## 3. The IV problem and the ablation

We have no implied vol for SUI and never will until we are liquid. The
agreed approach — backtest on BTC with IV estimated from realized vol,
and compare against the same bot using true IV — is the right experiment.
It isolates the cost of the missing input.

Measured from DVOL vs realized vol on our own lake:

| Year | avg DVOL | avg fwd-30d RV | IV / fwd RV | IV / trailing-30d RV |
|---|---|---|---|---|
| 2021 | 0.918 | 0.738 | 1.304 | 1.282 |
| 2022 | 0.761 | 0.695 | 1.141 | 1.170 |
| 2023 | 0.485 | 0.397 | 1.271 | 1.202 |
| 2024 | 0.577 | 0.509 | 1.187 | 1.169 |
| 2025 | 0.460 | 0.398 | 1.226 | 1.165 |
| 2026 | 0.441 | 0.414 | 1.302 | 1.111 |

Full sample: **IV = 1.18 × trailing RV (median), 1.235 × forward RV, and
IV exceeded subsequent realized on 73.4% of days.**

So "IV ≈ 1.2 × RV" is well calibrated as a central estimate. The problem
is dispersion: the ratio ranges 1.11–1.30 by year, so a constant
multiplier misses by roughly ±0.08 × RV. **At SUI's σ ≈ 0.87 that is ±7
vol points against a planned base spread of 4–6 vol points** — the
estimator error is larger than the entire intended edge. Expect the
ablation to conclude the multiplier must be regime-adaptive, not a
constant.

**Conceptual caution.** On BTC the ablation asks "how close is my
estimate to market IV." On SUI there is no market IV to be wrong about —
we set the price. The operative question is "does the hedged book make
money at the price I chose," which the ablation does not answer. A good
BTC ablation result must not be read as validation for SUI.

DVOL is also a 30-day ATM index: no skew, no term structure. For OTM
calls, BTC call skew is where the P&L difference lives. `00-plan.md`
already anticipates this ("skew/term shaped from BTC/ETH surfaces as
priors scaled by vol ratio").

---

## 4. Which realized vol to price off — per asset, not global

Volatility signature, Aug 2025 – Jul 2026, 1m Vision klines:

| sample | SUI | BTC |
|---|---|---|
| 1m | 1.270 | 0.459 |
| 15m | 0.916 | 0.443 |
| 1h | 0.857 | 0.429 |
| 4h | 0.862 | 0.418 |
| 1d | 0.870 | 0.430 |

BTC is flat at every interval — deep book, negligible microstructure
noise. **SUI's 1m RV is inflated ~45%.** A bot pricing off 1m RV would
overpay ~45% on vol for every option, which swamps every other parameter
in the system. SUI flattens at ≥15m.

**Design requirement this implies:** a bot "generalized across multiple
assets" must derive its sampling interval per asset from that asset's own
signature. `config.staging.toml` currently sets
`vol_sample_interval_ms = 300_000` — a fixed 5m for every underlying.
Adequate for BTC, wrong for SUI, unknown for whatever lists third.

**The 2025-10-10 cascade is in the data**: SUI printed a −0.55 log return
in one minute and −1.33 over five, then recovered. That is the scenario
that kills a band hedger, which rebalances into the wick. Any backtest
must include the date rather than sample around it.

**Funding is a small tailwind.** SUIUSDT, 636 settled intervals: mean
**+3.3% annualized**, median +4.0%, positive 71.7% of the time — shorts
receive. Note `hedge.rs`'s `funding_widen_threshold: -0.25` (widen bands
when shorts pay >25%/yr) will essentially never fire at these levels.
Bands should widen on **turnover cost**, not funding.

---

## 5. THE CORRECTION: band width is mis-specified

30d ATM call on SUI, 30% NAV premium budget (M = 3.0× NAV notional),
rolling windows over Aug 2025 – Jul 2026. "P&L" is option + hedge path
P&L excluding the spread edge.

| hedge policy | initial | rebalancing | ratio | P&L std | worst |
|---|---|---|---|---|---|
| unhedged | 0 | 0 | — | 20.1% | −30.0% |
| hedge once at entry | 1.66× | 0 | — | **24.0%** | −29.4% |
| bands 20% | 1.66× | 9.6× | 6× | 6.6% | −18.0% |
| bands 1.5% | 1.66× | 74.7× | **45×** | 5.8% | −15.6% |

Two things fall out.

**Maintenance is unavoidable.** Hedging once at entry produces a *higher*
P&L std than not hedging at all — in a trending market the static hedge
decays into a mismatched position (the call's delta runs to 0 while you
remain short 0.5). Against a 1.72% edge, unhedged noise of 20% is ~12×
the edge. There is no "hedge on acquisition and forget" option.

**But far less maintenance is needed than planned.** Full band sweep:

| band | turnover | P&L std | cost @1.0bp | @3.5bp | @4.5bp | break-even edge @3.5bp |
|---|---|---|---|---|---|---|
| 30% | 8.3× | 6.6% | 0.08% | 0.29% | 0.37% | 0.8p |
| **20%** | **11.3×** | **6.6%** | **0.11%** | **0.39%** | **0.51%** | **1.1p** |
| 10% | 19.1× | 6.0% | 0.19% | 0.67% | 0.86% | 1.9p |
| 5% | 33.1× | 6.0% | 0.33% | 1.16% | 1.49% | 3.4p |
| 3% | 48.5× | 5.9% | 0.49% | 1.70% | 2.18% | 4.9p |
| 1.5% | 76.4× | 5.8% | 0.76% | 2.67% | 3.44% | 7.8p |

Break-even edge = σ × turnover × fee / 0.30, i.e. vol points of spread
needed purely to pay the hedge bill — directly comparable to the plan's
4–6 vol point base spread.

**Risk reduction saturates at 10–20% bands.** Below that we pay to shave
a component already an order of magnitude smaller than the irreducible
one: P&L std floors at ~5.8–6.6% no matter how tightly we hedge, and that
floor is the **vol P&L (realized vs priced variance), which delta hedging
cannot remove**.

This is the standard result — optimal hedge bandwidth scales roughly as
(cost/gamma)^⅓, so tight bands are only correct when costs are near zero.
`00-plan.md` lists ±1.5% NAV under "V1 starting parameters (revisit after
60–90 days)" with no cost model behind it.

**Action:** `hedge.rs` defaults `band_pct_nav: 1.5` / `band_wide_pct_nav:
2.5` should become roughly `15` / `25`. Revisit before launch, not after.

**Honest costs of wide bands:** up to 20% of NAV carried as unhedged
delta between rebalances — a real risk-limit and depositor-optics
question even though the P&L std says it is cheap — and a modestly worse
tail (−18.0% at 20% bands vs −15.6% at 1.5%). Gap risk is where tight
bands earn their keep, and 2025-10-10 is the test case.

---

## 6. Venue economics

### 6.1 Fees (mainnet, verified 2026-08-15)

| | Bluefin Pro | Aftermath |
|---|---|---|
| place / cancel / reprice | **free** (off-chain sequencer) | **Sui gas per operation** (on-chain CLOB) |
| maker fill | 0.010% (1.0 bp) | **−0.005% (0.5 bp rebate)** |
| taker fill | 0.035% (3.5 bp) + **flat 0.03 USDC** | 0.045% → 0.026% at volume |
| matching | off-chain, <500ms | fully on-chain Move |
| SUI market | — | 10× max leverage, 5% MMR, **isolated** margin |

**Correction to `01-perps-venues.md`:** it records "SUI taker 0.1%".
Current Bluefin docs show no per-market exception — 0.035% flat. That is
3× too pessimistic. (A zero-fee SUI-PERP promo existed per an Aug-2024
tweet; it is not in current docs and should not be priced on. Aftermath's
maker rebate is likewise flagged as "an active promotion.")

The flat $0.03 taker fee only bites at tight bands — 0.12 bp per
rebalance at 20% bands on a $10k vault, 1.34 bp at 1.5% bands. It is a
flat fee, so it punishes small trades: another independent reason the
1.5% band is wrong.

### 6.2 Bluefin vs Aftermath

At 10–20% bands the fee difference is worth ~1 vol point of break-even
edge either way, so **fees do not decide this**. The real differences:

| | favours |
|---|---|
| Free repricing (a maintained ladder reprices far more often than it fills) | **Bluefin** |
| Depth — $40–70M/day, $40B cumulative, vs Aftermath launched later | **Bluefin** |
| Trustless on-chain NAV appraisal of the hedge position | Aftermath |
| Atomic exercise + hedge unwind in one PTB | Aftermath |
| No hedge-signer / no service-liveness dependency | Aftermath |
| Counterparty risk — Aftermath exploited for $1.14M, 2026-04-29 | **Bluefin** |

Aftermath's rebate is paid on *fills*; its gas is charged on *attempts*.
For a passive strategy with a low fill-to-attempt ratio, that is the
wrong side of the trade.

**Decision (2026-08-15):** trust properties are not a constraint while
this runs on prop capital, and Bluefin is already wired. **Bluefin for
the hedging loop.** The exercise loop pulls the other way (§9.3) but the
cost is ~0.05% of notional, so it does not overturn the call. Keep the
`HedgeVenue` trait as the seam; revisit if outside capital changes the
NAV-appraisal calculus.

---

## 7. Passive hedging: the ladder

The hedge does **not** depend on predicting exchange flow. Two distinct
flows: options flow from our exchange (unpredictable, possibly zero) and
hedge order flow on the perp venue (the bot's own trading, driven by
inventory it already holds). Zero exchange flow means zero inventory,
zero delta, nothing to hedge, zero cost. **The strategy has no fixed cost
when idle.**

Given inventory, delta is a known function of spot and time, so the
rebalance levels can be solved and pre-placed:

```
inventory: long calls, M×NAV notional, current delta d, hedged h
  P_up   = price where (delta(P) − h)·M = +band  → resting SELL (short more)
  P_down = price where (delta(P) − h)·M = −band  → resting BUY  (cover)
```

Price trades through a level → fill **at the limit price** → that fill
*is* the rebalance. Recompute both levels and re-rest; reprice on every
fill, every inventory change, and as T decays.

**This is not a coincidence.** Long gamma means sell-as-it-rises,
buy-as-it-falls — mechanically identical to passive two-sided quoting.
Because the bot buys calls it is structurally long gamma, so its hedge is
naturally passive-executable. (A short-gamma book must buy high and sell
low, which resting orders cannot express — short-gamma hedging is
intrinsically taker. Relevant for V2.)

Gaps are survivable: a resting order gapped through still fills at *our*
price, better than where the market ended up. The failure mode is running
out of rungs, so size the ladder for a large move, not just the next band.

**What it is worth** (both modes filling at the same trigger price;
passive saves the fee difference plus the half-spread it does not cross),
per 30d turn against the 1.72% edge:

| band | turnover | spread 1bp | 2bp | 5bp | 10bp |
|---|---|---|---|---|---|
| 30% | 8.3× | 0.25% | 0.29% | 0.41% | 0.62% |
| **20%** | **11.3×** | **0.34%** | **0.39%** | **0.56%** | **0.84%** |
| 10% | 19.1× | 0.57% | 0.67% | 0.96% | 1.44% |
| 5% | 33.1× | 0.99% | 1.16% | 1.65% | 2.48% |
| 1.5% | 76.4× | 2.29% | 2.67% | 3.82% | 5.73% |

At 20% bands that is 20–49% of the edge depending on the true spread —
material, worth building, not existential. At 1.5% bands it is 130–330%
of the edge, i.e. the difference between working and not. **Band width is
first-order; execution mode is second-order.**

Caveats: passive hedging is lagged hedging (ladder P&L std rose to 7.4%
vs 6.7% at 20% bands); whipsaw, not gaps, is the real enemy (fill then
revert = trade back, paying twice — wide bands are the defence); queue
position was assumed away; and **Bluefin's actual SUI-PERP spread is
unknown**, with the table spanning 2.5× across plausible values.

---

## 8. Capital velocity and cost of capital

Gross edge per turn is a clean identity:

```
edge_per_turn = premium_budget × X / σ        (X = spread in vol points)
              = 0.30 × 0.05 / 0.87 = 1.72% of NAV      — independent of tenor
```

Capital required per **$1/day** of retail call notional absorbed:

| tenor | premium % notional | held to expiry | resold in 7d | resold in 3d |
|---|---|---|---|---|
| 7d | 4.82% | **1.1×** | 1.1× | 0.5× |
| 14d | 6.82% | 3.2× | 1.6× | 0.7× |
| 30d | 9.98% | **10.0×** | 2.3× | 1.0× |
| 60d | 14.12% | 28.2× | 3.3× | 1.4× |

Inverted, per $1,000 deposited: weekly buckets held to expiry absorb
~$325k/yr of retail notional; monthlies ~$37k/yr.

**The reframe: capital velocity is almost certainly not the binding
constraint — retail demand is.** Even $1,000 of NAV absorbs $37k/yr of
flow on the worst configuration. Plan to be flow-starved, not
capital-starved, for a long time. Optimise for edge and survivability,
not velocity.

**Bucket schedule.** Weeklies are ~9× more capital-efficient per unit of
flow but carry ~2× the hedge turnover per unit of premium (148.6× vs
76.8×), because gamma concentrates into expiry. Which wins is decided by
execution mode: passive hedging makes weeklies excellent; pure taker
hedging makes them unviable (break-even edge 15.1p at 1.5% bands, 9.7p at
5%). The current-week bucket inside its final 48h is where turnover and
gamma explode — `00-plan.md`'s "near-expiry ATM throttle (2× spread, ½
size inside 48h)" is not an optimisation, it is the most important
control in the ladder.

---

## 9. American exercise and the flash-loan flow

### 9.1 DeepBook flash loans are free

From the vendored source (`contracts/vendor/deepbook/sources/vault/vault.move`):

```move
assert!(coin.value() == flash_loan.borrow_quantity, EIncorrectQuantityReturned);
```

Repay exactly what was borrowed. **No fee, no interest.** The only gate
is pool inventory (`ENotEnoughQuoteForLoan`).

The cost is entirely the swap leg. DeepBook v3 governance constants
(`state/governance.move`):

| pool type | taker (default → min) | maker |
|---|---|---|
| volatile (SUI/USDC) | **10 bps** → 1 bp | 5 bps → 0 |
| stable | 1 bp → 0.1 bp | 0.5 bps → 0 |

Defaults are the *max* of each range; governance can vote them down.

### 9.2 What it costs

30d ATM calls on SUI, M = 3.0× NAV:

| | |
|---|---|
| P(ITM at expiry) | **33%** |
| E[intrinsic] / notional | 2.86% |
| E[exercised notional] | **0.99× NAV per turn** |

**DeepBook is the execution floor, not the execution plan.** The vault
will integrate a swap router, so DeepBook is the worst case and anything
better is upside. Measured 2026-08-15, selling SUI for USDC (the
exercise-unwind direction), DeepBook book-walk vs the Aftermath router:

| size | DeepBook only | Aftermath router | router route |
|---|---|---|---|
| $10k | **13 bp** | 20 bp | Cetus, Obric |
| $50k | 41 bp | **31 bp** | Bluefin, Cetus |
| $100k | 50 bp | **35 bp** | Bluefin, Cetus |
| $250k | 80 bp | **39 bp** | Bluefin, Cetus |
| $500k | **exhausted** | **47 bp** | Bluefin, Cetus |
| $1M | **exhausted** | 238 bp | Bluefin, Cetus |

Three consequences:

- **The router is not always better.** Below ~$25k, DeepBook direct wins
  — the router adds hops and protocol fees. The vault should choose per
  trade, not route unconditionally.
- **Sui spot liquidity is not in DeepBook.** Every router path above
  goes through Bluefin and Cetus. DeepBook is the flash-loan source and
  the fallback, not where the size actually clears.
- **Capacity extends ~5–10× versus DeepBook alone**, to somewhere past
  $1M rather than ~$500k.

Exercise cost per turn (E[exercised notional] ≈ 1× NAV, so slippage in bp
≈ cost in bp of NAV), against the 1.72% edge:

| vault NAV | best execution | cost %NAV/turn | vs edge |
|---|---|---|---|
| $10k | 13 bp | 0.13% | 8% |
| $100k | 35 bp | 0.35% | 20% |
| $500k | 47 bp | 0.47% | 27% |
| **$1M** | **238 bp** | **2.36%** | **137%** |

**At ~$1M NAV a single expiry unwind costs more than the entire round-trip
edge.** Below ~$500k the exercise flow is a quarter to nine-tenths of the
hedge bill and breaks nothing. That is the real capacity ceiling and it
is closer than the fee schedules suggest.

Two caveats. This assumes the whole exercised notional is dumped at once;
laddering across the expiry window and letting depth replenish raises
capacity materially (`00-plan.md` already calls for laddering big size).
And it is one snapshot of one book — the shape is trustworthy, the
specific basis points are not.

The structural point stands: we borrow and swap ~100% of notional to
harvest ~2.9% of notional. Break-even swap cost is ~870 bps against
average intrinsic-when-ITM of 8.7% — a large cushion per trade — but the
cost lands on NAV every turn, which is why the table above binds before
the per-trade economics do.

### 9.3 Five consequences

1. **Minimum-moneyness exercise threshold.** Exercise only if intrinsic >
   total swap cost. At 35 bps all-in: exercise iff spot > strike ×
   1.0035. Below that, abandon — paying 35 bps of notional to harvest
   10 bps of intrinsic is a pure loss. Belongs in the keeper.
2. **Two separate ceilings — and a router only lifts one of them.**
   - *Swap slippage* (selling the underlying): fixed by routing. §9.2.
   - *Flash-loan borrow* (sourcing the strike): **not fixed by routing.**
     `borrow_flashloan_quote` asserts
     `self.quote_balance.value() >= borrow_quantity`, so a single venue
     must hold the entire strike notional — a $1M vault at M = 3.0×
     needs $3M of USDC in one pool. A swap router aggregates *execution*,
     not *borrow*. Lifting this needs either a flash-loan aggregator or a
     lender (Navi / Scallop / Suilend), all of which charge a fee where
     DeepBook charges zero.

   Note `quote_balance` is the pool's total deposited quote, not its
   resting bids, so the borrow ceiling is higher than the visible book —
   but it is unmeasured, and the orderbook endpoint does not expose it.
   **This is the least-understood constraint in the whole design.**
3. **The unwind is two legs and they cannot be atomic on Bluefin.** At
   exercise the bot holds physical SUI while still short perp; selling
   the SUI leaves it net short, so the perp must also close. DeepBook
   taker (10 bps) **plus** Bluefin taker (3.5 bps) plus both slippages.
   `00-plan.md`'s "the exercise sale and hedge unwind are the same trade"
   is not achievable with off-chain matching. Exposure between legs is
   ~0.05% of notional per 10s at SUI's vol — engineer it tight.
4. **This is the one real argument back toward Aftermath**, where the
   whole flow is one PTB: flashloan → exercise → DeepBook swap → close
   perp → repay. Small enough not to overturn §6.2.
5. **Exercise is bursty and calendar-synchronised.** Long calls are
   almost never worth exercising early absent a large dividend (SUI
   staking is low single digits and the collateral is not ours to
   stake), so effectively all exercise happens at expiry. `public-docs`
   is explicit that there is no settlement window — after expiry
   "holders' unexercised coins are worthless" — so the keeper must act
   *before* the timestamp on intrinsic that is still uncertain, and every
   position in a bucket hits at once. Given (2), **stagger expiry times,
   not just expiry dates.**

---

## 10. Assembled P&L, and what actually matters

Per 30-day turn, wide-band configuration:

```
edge (5 vol pts, 30% premium budget)   +1.72% NAV
hedging @ 20% bands, passive           −0.11%
hedging @ 20% bands, taker             −0.39%
exercise swap + perp unwind            −0.10%  to −0.45%
─────────────────────────────────────────────────────
net                                    +0.9%   to +1.5% NAV per turn
```

**And the term that dwarfs all of them:** across every hedged variant the
mean path P&L was **−2.7% per turn**. On this sample, pricing at trailing
daily RV *overpaid* for vol — realized variance came in below what the
bot paid. That single term is larger than the entire execution stack
above.

The conclusion for the build: **the IV estimator and bid discipline are
the strategy. Hedging and exercise mechanics are rounding errors against
them.** `00-plan.md` already says the two things to build most carefully
are the theta governor and bid-pricing discipline; this is quantitative
support for that ordering.

### The resale assumption

The strategy's economics improve sharply with faster resale (30d resold
in 3d absorbs 368× NAV/yr vs 37× held to expiry). But **at launch the buy
page has no resting bids** — the bot's ask is the only order in the book.
Liquidation is therefore impossible, not merely expensive; resale is
purely a fill-probability question against retail buy-side demand that
does not yet exist.

Consequences: every short-holding-period row in this document is
aspirational; the realistic launch case is `hold = tenor`. **Design so
the no-resale case survives and treat resale as upside.** And note that
"we set the spread on both sides" is weaker than it looks — we control
the ask *price*, not whether anyone pays it. The only price genuinely
controlled is the **bid on the Earn page**.

---

## 11. Corrections to existing docs

| Doc | Says | Should say |
|---|---|---|
| `00-plan.md` | delta band ±1.5% NAV (2.5% wide) | ~15% / 25% — §5 |
| `hedge.rs` | `band_pct_nav: 1.5`, `band_wide_pct_nav: 2.5` | same |
| `hedge.rs` | `paper_slippage_bps: 5.0` | 3.5 bp taker + spread; sweep it |
| `01-perps-venues.md` | Bluefin "SUI taker 0.1%" | 0.035% flat — §6.1 |
| `config.staging.toml` | `vol_sample_interval_ms = 300_000` global | per-asset from signature — §4 |
| `00-plan.md` | "exercise sale and hedge unwind are the same trade" | not achievable on Bluefin — §9.3 |
| memory: data-room | "no strike-level IV" | Deribit chain live since 2026-08-14 — §1.1 |

---

## 12. Open questions

1. **What APY do we intend to promise depositors?** That is the hurdle
   rate everything above must clear, and it is in no document.
2. **What is Bluefin's actual SUI-PERP spread and depth?** The single
   measurement that would sharpen §7 most; currently unknown, and the
   answer moves the passive-execution value by 2.5×.
3. **What is the flash-loan borrow ceiling?** Per §9.3(2) this is the one
   constraint a swap router does not lift, and `quote_balance` is not
   exposed by the indexer's orderbook endpoint. Needs an on-chain read of
   the pool object. Least-understood constraint in the design.
4. **At what size does routing beat DeepBook direct?** §9.2 puts the
   crossover near $25k on one snapshot. The vault needs this as a live
   decision rule, not a constant.
4. **Does the orderbook ask get repriced, or posted once?** Affects
   whether resale is a maintained quote or a standing offer.

## 13. What to build

1. **SUI vision backfill** (config-only) + `parse_book_ticker_csv`
   adapter for the 2023-05 → 2024-04 BBO window.
2. **Collectors on Bluefin and Aftermath SUI-PERP books, plus DeepBook
   SUI/USDC** — starts the clock on the three unknowns above. Every day
   without this is history we cannot recover.
3. **Hedge sim against tick data**, with **passive fill fraction** as the
   primary swept axis, plus band width, tenor and holding period. Output
   break-even surfaces, not point estimates. Gas as an explicit
   per-rebalance fixed cost so the small-vault regime is visible.
4. **The IV ablation** (§3) on BTC, once the sim exists.

---

## 14. Methodology and caveats

All figures from Binance Vision 1m klines for SUIUSDT and BTCUSDT perp,
**Aug 2025 – Jul 2026** (525,600 bars each), and the data room's own
DVOL/bars/rv tables. Option maths is Black-Scholes on ATM strikes, 30%
NAV premium budget, rolling windows every 5–15 days.

Known limitations, in rough order of importance:

- **Fees only, zero slippage.** No depth data for any venue we would
  actually trade. At scale this is the dominant omission.
- **One asset, one 12-month regime** — SUI −81%, BTC −46%, including the
  2025-10-10 cascade. The option and hedge P&L legs are period-specific;
  the *turnover* and *cost* columns are robust because they are driven by
  path, not direction.
- **σ fitted in-sample** over the whole window, so the mean P&L figures
  carry lookahead. Break-even and turnover figures do not depend on it.
- **Single position, not a running book.** A real book rebalances *net*
  delta across many positions, so these numbers likely overstate
  turnover somewhat.
- ATM only; no skew, no strike ladder, no partial fills, no queue
  position, no gas in the headline tables.
- The band-saturation *shape* (§5) is solid theory and should be trusted
  directionally; the specific break-even numbers are one sample and
  should be re-derived across regimes before setting production
  parameters.
