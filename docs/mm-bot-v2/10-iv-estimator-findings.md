# IV estimator findings (doc 10 — G6, SO-441)

Status: **SECOND PASS 2026-09-02** (§4.2 adds the G5 forecaster). First pass 2026-09-02. Produced by `desk-backtester` v0
(SO-439) against the lake mirror. Every number below is a conditional
simulation: `proxy_oracle`, `proxy_venue` (Binance spot/perp as the path
and the mark), `taker_only`, `no_resale`, `constant_flow`, `exercise=at_expiry`.
Nothing here is an APY claim (doc 08 §0.2, §12).

## 0. TL;DR

0. **On BTC, paying market IV loses 77 percent of NAV in 4.5 years;
   pricing off realized vol ends flat.** The buyer's asymmetry from doc 09
   §2.1, measured (§4). The estimator's target is realized vol, never IV.
1. **The thin slice reproduces doc 07 §5.** At the 20% band the hedge
   turns over 11.4–13.5× NAV per 30 days (doc 07: 11.3×); at 1.5%,
   59–67× (doc 07: 76×); at 30%, 7–10× (doc 07: 8.3×). Fees and slippage
   scale with it exactly as doc 07's break-even table says. The engine is
   fit for the band and cost questions.
2. **On Aug 2025 – Jul 2026 SUI the desk bought vol cheap, not rich.** The
   struck sigma averaged 0.63–0.68 against 0.87 realized over each
   option's life (15-minute sampling). This is the opposite sign to doc 07
   §10's −2.7 percent per turn, and the difference is methodology: doc 07
   fitted σ in-sample over the whole window (its own §14 caveat), this run
   prices off the trailing two-window blend the live desk uses. On this
   sample the max-lean lift (§2.3 of doc 09) barely matters: ±3 percent of
   NAV at the end of the year.
3. **The risk premium is the bigger lever than the blend.** Dropping the
   +5 vol-point `risk_premium` (which the base spread then removes) moves
   the year-end NAV by 15–30 percent of NAV at every band, because it
   changes what the desk pays; the lean changes only how fast the surface
   reacts. Both are estimator choices, and neither is the HAR forecaster
   yet (G5 lands separately; this doc is re-run when it does).
4. **Regime warning.** SUI fell 82 percent in the window. A long-call,
   short-perp book is structurally short the underlying and the hedge leg
   made 1.9 million on a 1 million NAV while the option leg lost 1.6. That
   is a one-regime result, exactly the kind doc 08 §9.2 says never to rank
   on. The BTC legs below and the walk-forward runner (PR O) are where a
   conclusion has to come from.

## 1. Setup

- Path: `gold/v1/bars/freq=60s/exchange=binance/symbol=SUI-USDT` (spot),
  backfilled for 2023-05 → 2026-08 (SO-459); funding
  `silver/v1/funding_rates/exchange=binance/symbol=SUI-USDT-PERP`
  (settled rows, 1095 in the window). Coverage 99.98 percent; the 2025-10-10
  cascade is in-sample.
- Desk: one 30-day ATM call position per turn sized at 3× NAV of spot
  notional (doc 07's M = 3 framing), fair − 5 vol points, no size penalty,
  30 percent premium cap in one expiry; taker hedge at 3.5 bp + 3.5 bp
  slippage + 0.03 flat; funding at every Binance settlement against the
  signed position; at-expiry settlement with 5 bp + 2.5 bp spot costs.
- Estimator: the live desk's two-window blend (24 h / 168 h) at 15-minute
  sampling (doc 07 §4), `max_lean` ∈ {0, 0.8}, `risk_premium` ∈ {0, 0.05}.
- Scenario file: `rust-backend/crates/backtester/scenarios/sui_doc07_calls.toml`;
  sweep: `desk-backtester sweep --bands 1.5,3,5,10,20,30 --risk-premiums 0,0.05 --max-leans 0,0.8`.

## 2. Doc 07 §5 reproduction — band sweep (SUI, calls, risk_premium 0.05, max_lean 0.8)

| band %NAV | turnover ×NAV / 30d | doc 07 | fees + slippage (of 1M NAV) | funding received | year-end NAV | max DD |
|---:|---:|---:|---:|---:|---:|---:|
| 1.5 | 60.2 | 76.4 | 513,623 | 47,890 | 838,273 | 0.441 |
| 3 | 42.6 | 48.5 | 363,122 | 51,573 | 1,004,878 | 0.379 |
| 5 | 31.7 | 33.1 | 270,420 | 54,426 | 1,133,372 | 0.338 |
| 10 | 19.1 | 19.1 | 162,378 | 55,122 | 1,164,013 | 0.325 |
| 20 | 11.8 | 11.3 | 100,098 | 57,564 | 1,330,342 | 0.341 |
| 30 | 7.2 | 8.3 | 61,213 | 54,414 | 1,150,506 | 0.342 |

Turnover matches doc 07 to within its own stated tolerance (§14: single
position, ATM, no partial fills). Cost per 30-day turn at 20 percent bands
is 0.83 percent of NAV here versus doc 07's 0.51 percent at 4.5 bp; the
difference is the flat 0.03 fee and the 3.5 bp slippage on top of the 3.5
bp taker fee, both of which doc 07 §6.1 lists but its table omits. The
band-saturation shape (risk reduction flattens at 10–20 percent) holds:
drawdown is flat from 5 percent outward.

## 3. Estimator bias (SUI, 9 fills, 8 settled)

| estimator | mean σ paid | mean σ realized (life, 15 m) | bias | vol-P&L proxy | year-end NAV (band 20) |
|---|---:|---:|---:|---:|---:|
| blend, rp 0.05, lean 0.8 (live desk) | 0.684 | 0.871 | +0.187 | +799,294 | 1,330,342 |
| blend, rp 0.05, lean 0 | 0.684 | 0.871 | +0.187 | +780,968 | 1,261,680 |
| blend, rp 0, lean 0.8 | 0.634 | 0.871 | +0.237 | +1,099,408 | 1,629,872 |
| blend, rp 0, lean 0 | 0.634 | 0.871 | +0.237 | +1,095,318 | 1,607,978 |

`σ paid` is the surface sigma minus the 5-point base spread; the surface
sigma itself averaged 0.68 with the premium and 0.63 without, against
0.87 realized. The desk under-bought vol on this sample at every setting,
so the doc 09 §2.3 max-lean concern did not bite here: after 2025-10-10 the
lean held the bid up for a few days, but realized vol stayed high for
longer than the 24-hour window anyway. Whether the lean hurts is a
question for calmer regimes; see §4.

## 4. BTC ablation (2022-01 → 2026-07, 56 turns, 30-day ATM calls, band 20)

Same desk, same costs, two sigma sources: the live two-window blend on
BTC-USDT-PERP bars at 5-minute sampling (`btc_rv_calls.toml`) versus the
Deribit DVOL index as the base ATM sigma (`btc_dvol_calls.toml`, doc 07
§3's "true IV" leg). Both bids are sigma − 5 vol points.

| sigma source | fills | mean σ paid | mean σ realized | bias | option leg | hedge realized | funding received | fees | gross return | max DD |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| realized-vol blend | 52 | 0.463 | 0.497 | +0.034 | +2,671,950 | −3,077,178 | 666,061 | 254,488 | −1.9 % | 0.428 |
| DVOL (market IV) | 55 | 0.551 | 0.498 | −0.053 | −500,450 | −455,469 | 285,653 | 92,000 | −76.8 % | 0.772 |

Three things fall out:

1. **Paying market IV is fatal for a buyer.** DVOL sat 5 vol points above
   realized on average (doc 07 §3: IV exceeded subsequent realized on 73
   percent of days) and the desk that priced off it lost three quarters of
   NAV over the sample even after the 5-point discount. This is the doc 09
   §2.1 asymmetry measured: overestimate σ and the loss is realized slowly
   and completely.
2. **The realized-vol blend was roughly unbiased on BTC** (+3 vol points in
   the buyer's favour) and the desk ended flat before edge over 4.5 years,
   with a 43 percent drawdown along the way: the gamma-scalp leg and the
   option leg largely cancel, and funding income (shorts received 666k on a
   1M book) paid the fees. Flat-before-edge is the right baseline for a
   bid-only desk: the edge has to come from the vol-point spread and the
   quantile choice, not from the estimator being clever.
3. **"Tracks market IV" is the wrong target.** The DVOL leg tracks the
   market perfectly and loses; the blend tracks realized and survives. The
   G5 forecaster's job is the second thing (doc 09 §2.1), and its quantile
   gate (§2.5) should be judged on realized, never on DVOL.

Caveats: 5-minute sampling for BTC (doc 07 §4 says any interval is fine on
BTC); the DVOL leg uses the 30-day index for a 30-day option, so term
structure is not the issue; both legs share the same proxy venue and
at-expiry settlement.

### 4.1 SUI mixed book, daily flow (`sui_mixed_daily.toml`)

$100k/day of spot notional, half calls half puts, 14-day board expiries,
doc 08 §0.4 caps, Aug 2025 – Jul 2026: 725 fills, 5 capacity declines,
σ paid 0.834 versus 0.859 realized (bias +0.025), gross −13.0 percent,
max drawdown 20.4 percent, hedge turnover 4.9× NAV per 30 days. The
put side removes most of the directional windfall the calls-only book
collected in the crash: the option leg lost 111k and the hedge made 3k.
This is the shape the capacity solver (PR N) will search over.

## 4.2 Second pass: the G5 HAR forecaster (`estimator.kind = "har"`, q_bid 0.35)

Same scenarios, the SO-440 forecaster (HAR-RV-CJ, per-asset sampling
interval, quantile bid at q = 0.35, no max-lean, no risk premium term
because the quantile replaces it) in place of the two-window blend.

| scenario | estimator | fills | mean σ paid | mean σ realized | bias | gross return | max DD | turnover ×NAV/30d |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| SUI calls, per turn, Aug 25–Jul 26 | windows (rp 0.05, lean 0.8) | 9 | 0.684 | 0.871 | +0.187 | +33.0 % | 0.341 | 11.8 |
| SUI calls, per turn | **har q0.35** | 8 | 0.613 | 0.926 | +0.314 | +21.8 % | **0.214** | 10.0 |
| BTC calls, per turn, 2022–2026 | windows | 52 | 0.463 | 0.497 | +0.034 | −1.9 % | 0.428 | 13.0 |
| BTC calls, per turn | **har q0.35** | 56 | 0.432 | 0.503 | +0.071 | **+65.7 %** | **0.247** | 18.0 |
| BTC calls, per turn | DVOL | 55 | 0.551 | 0.498 | −0.053 | −76.8 % | 0.772 | 4.7 |
| SUI mixed daily, Aug 25–Jul 26 | windows | 725 | 0.834 | 0.859 | +0.025 | −13.0 % | 0.204 | 4.9 |
| SUI mixed daily | har q0.35 | 726 | 0.843 | 0.859 | +0.016 | −21.8 % | 0.346 | 4.5 |

Reading:

- **On BTC the forecaster is the difference between flat and profitable.**
  Over 56 turns it bid 7 vol points under realized on average (the
  quantile doing its job), took every turn (zero capacity declines
  because it never chased spikes into the cap), and cut the drawdown from
  43 to 25 percent. This is the first result in this document that clears
  the doc 08 §0.4 hurdle on a multi-year sample, and it is still one
  asset and one estimator setting; walk-forward folds (PR O) decide
  whether it holds.
- **On SUI calls it traded return for drawdown.** Lower struck sigma
  (0.61 versus 0.68) and one more capacity decline; the crash regime paid
  the aggressive blend more, but the forecaster's book drew down 21
  instead of 34 percent. That is the buyer's asymmetry again: bidding low
  costs volume, not money.
- **The mixed book is not an estimator problem.** Both estimators price
  the put side within 2 vol points of realized and both lose; the put leg
  loses on delivery costs and the hedge whipsaw in a −82 percent year. It
  is a hedge-band and put-exercise question for PRs L and M, not a σ
  question.

## 5. Recommended defaults (second pass)

- Adopt `estimator = "har"` with `q_bid = 0.35` as the staging default
  once the walk-forward runner (PR O) confirms the BTC result out of
  sample; until then it runs in shadow on `/desk/state` (SO-440 wires it
  behind a flag, default `windows`).
- Drop the max-lean lift: it never helped, and the forecaster's post-shock
  regime replaces it.
- Keep `risk_premium` at 0 under `har` (the quantile is the premium).
- Keep bands at 15–20 percent; the forecaster does not change the doc 07
  turnover picture.

## 5.1 Recommended defaults (first pass, superseded)

- Keep `band_pct_nav` 15–20 and `band_wide_pct_nav` 25 (SO-436 already
  applied); nothing here argues for tighter.
- Do not change `risk_premium` from this run alone: on this sample zero
  premium wins by construction (the desk paid less for vol that realized
  higher), and a bull regime will say the opposite. The G5 forecaster's
  quantile replaces both `risk_premium` and the base spread as the
  bid-discipline knob; adopt it after the walk-forward folds exist.
- Sampling interval 900 s for SUI stands (doc 07 §4).

## 6. Caveats

- One asset-year, one regime (−82 percent). No walk-forward, no holdout.
- At-expiry settlement only; no early exercise, no resale (doc 08 P3/P4).
- Perp mark = spot; no basis, no margin model (proxy_venue).
- Constant flow: 9 fills, 4 capacity declines (premium above the 30
  percent cap when σ spiked). Not a demand model.
- Realized vol per option is measured on the same 15-minute decision-price
  samples the estimator sees; a sub-interval measure would read higher for
  SUI (doc 07 §4 signature).
