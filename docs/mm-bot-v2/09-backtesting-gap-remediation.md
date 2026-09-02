# Backtesting framework — gap remediation plan

Status: **PROPOSED 2026-09-01.** Follow-up to `08-backtesting-framework.md`
after a review of the plan and of the merged PRs A–F. It does not replace
doc 08; it reorders it, fills the gaps the review found, and specifies the
one component doc 08 never gave a phase: the IV estimator.

Scope (confirmed 2026-09-01): the bot is a Trader MM on the core protocol's
Earn page. It buys covered calls and cash-secured puts from retail writers,
never sells or writes an option, and hedges the resulting signed delta
with perps. Exchange market making is out of scope. Doc 08 §0.1 already
describes exactly this desk.

Standing decisions this plan inherits or adds:

| Decision | Date |
|---|---|
| No own-exchange or testnet RFQ data is calibration data; the PR A recorder gets a default-off flag (not yet implemented) | 2026-09-01 |
| Oracle-agnostic: everything supports both Pyth and Switchboard; no data specific to one oracle is used anywhere in pricing, backtesting, or calibration; the mainnet oracle choice is deferred and nothing plans around it | 2026-09-01 |
| Free data only; no vendor tick history | 2026-08-22 |
| DeepBook flash capacity is a labeled assumption until a pool-balance poller exists | 2026-08-22 |

The oracle decision supersedes doc 08 §1.6 and §6.1 wherever they name
Pyth as the live path or synthesize Pyth-specific fields. Section 3 below
restates those in oracle-neutral terms.

---

## 1. What the review found

The full review is in the 2026-09-01 session; the findings that drive this
plan are:

1. **The bid has no phase.** Doc 07 §10 shows the vol term (priced minus
   realized variance) dwarfs every execution cost, and doc 08 puts the IV
   study last. For a one-sided buyer the bid is the strategy.
2. **Sequencing front-loads infrastructure.** P1–P3 (workspace merge,
   kernel extraction, exact ledger, causal clock, Bluefin lifecycle) all
   precede any economic output. The kernel it extracts does not exist yet;
   live desk logic is spread across I/O tasks.
3. **Merged work meets its gates at the unit level only.** Nothing tests
   the wired runtime paths. Specific defects: PR D charges each fill on its
   own delta and ignores the existing perp position; venue fees and fixed
   cost are hard-coded to zero; the doc 07 band correction (1.5/2.5 → 15/25)
   was never applied and is live on staging; the paper venue never accrues
   funding as P&L; PR F ships three of nine metrics and no throttle; PR E
   and PR G do not exist.
4. **Two protocol costs are missing.** The core protocol skims `fee_bps`
   from gross premium on every write (desk pays gross). The trading vault
   charges a curator performance fee and a protocol cut. The return hurdle
   never says whether it is desk gross or depositor net.
5. **The surface's blend is biased the wrong way for a buyer.** See §2.3.
6. **Put-side data is nearly empty.** No buy-base ladder, no funding
   settlement poller, S5 silver not started, Bluefin capture under three
   weeks old. Doc 07 measured calls only.
7. **One oracle-specific data path exists.** `oracle-service /vol/realized`
   computes realized vol from Pyth Benchmarks daily closes. It violates the
   oracle-agnostic rule and must not feed the estimator.

---

## 2. The IV estimator

### 2.1 Purpose

The desk quotes into a market with no implied volatility. On Deribit a
maker's vol estimate is checked every second by other makers; here the bid
is set by one bot with no competitor, and the writer either accepts it or
walks. The only thing that corrects a wrong bid is realized P&L, months
later.

The bid is `fair(σ_model) − base spread − size penalty − inventory penalty
− expected hedge cost`. Every term after the first is a policy choice
measured in vol points. `σ_model` is a forecast, and its error is measured
in the same units. Doc 07 §3 puts the year-to-year dispersion of IV over
realized at roughly ±8 percent of vol, which at SUI's σ ≈ 0.87 is about
±7 vol points against a planned base spread of 4–6. **The forecast error is
larger than the intended edge.** That is why the estimator is the
strategy and the hedge is a rounding error.

The desk is long vol. Its P&L over an option's life is, to first order,

```text
vol P&L ≈ ½ · Γ · S² · (σ_realized² − σ_paid²) · τ
```

so the sign of `σ_realized − σ_paid` is the sign of the trade. The
estimator's job is to make that difference positive in expectation and to
tell the bid how uncertain it is.

The asymmetry matters and it is the opposite of a seller's:

- **Overestimate σ** and the desk overpays for every option. The loss is
  realized slowly as theta that gamma scalping does not recoup. This is
  the doc 07 result: −2.7 percent of NAV per turn on one sample.
- **Underestimate σ** and the bid is too low. If writers have any price
  elasticity, flow declines. No money is lost; volume is.

A buyer should therefore bias low and let elasticity find the volume, not
bias high and hope realized catches up. Section 8.4 of doc 08 already
makes elasticity a swept axis; the estimator has to supply the quantile
that sweep moves.

### 2.2 What it must do

The estimator is a pure function shared by the live desk and the
backtester (doc 08 §1 item 1, parity). It replaces the current two-window
blend in `pricing::surface::VolSurface::from_windows`.

```text
forecast(asset, price_history, horizon τ, now) -> VolForecast {
    sigma_mean:      f64,          // expected annualized realized vol over [now, now+τ]
    sigma_quantile:  fn(q) -> f64, // distribution, not a point
    regime:          Regime,       // calm | elevated | post-shock | cold
    sample_interval: Duration,     // the interval actually used for this asset
    coverage:        f64,          // fraction of the lookback with usable data
    staleness_ms:    u64,          // age of the newest observation
}
```

Requirements, each with a gate in §2.5:

1. **Forecast forward realized vol over the option's life, not trailing
   vol.** Trailing RV is an input, never the answer. Horizon-aware: a
   7-day option and a 60-day option get different forecasts from the same
   history.
2. **Per-asset sampling interval from the volatility signature.** Doc 07
   §4: SUI's 1-minute RV is inflated ~45 percent by microstructure noise
   and flattens at ≥15 minutes; BTC is flat at every interval. The
   estimator derives the interval per asset from the signature (first
   interval where RV is within a tolerance of the 1-hour value), never from
   a global config. `vol_sample_interval_ms` becomes a derived value.
3. **Multi-horizon components.** HAR-RV structure: daily, weekly, and
   monthly realized components with fitted weights, refit walk-forward.
   This is the standard forward-RV forecaster and it is cheap.
4. **Jump and gap handling.** 2025-10-10 (−0.55 log return in one minute)
   is in the data and must stay in the data. Separate the continuous
   component from jumps (bipower variation), forecast both, and report
   them separately so the bid can charge for tail risk explicitly instead
   of smearing one wick across a month of quotes.
5. **A distribution, not a point.** Report quantiles from the walk-forward
   residual distribution. The bid uses `sigma_quantile(q_bid)` where
   `q_bid` is a policy parameter the backtester sweeps (start at 0.30 to
   0.40). This is the concrete form of "bid discipline".
6. **Regime state.** Calm, elevated, post-shock, cold. Post-shock is the
   regime where trailing RV overstates forward RV most; the forecast must
   mean-revert faster than a trailing window does. Cold means insufficient
   history; the surface falls back and labels it.
7. **Skew and term from the asset's own distribution first.** Realized
   variance is strike-independent, but the payoff of an OTM option is not:
   what matters for an OTM call after a run-up is the conditional tail.
   Derive convexity from the asset's empirical return kurtosis and jump
   intensity. Borrowed BTC/ETH skew priors are a forward-only overlay
   validated against the Deribit chain (which only starts 2026-08-14), not
   a historical input.
8. **Oracle-agnostic inputs.** Live: the oracle-service price cache, which
   already serves the same keys from either provider. Historical: lake
   mids (Binance Vision proxy, Coinbase, Bluefin). Never Pyth Benchmarks,
   never Switchboard-specific history. The `/vol/realized` endpoint is
   retired from the pricing path.
9. **Deterministic and cheap.** Same inputs, same output, byte-identical.
   Fits are refit on a schedule (daily), not per quote.

### 2.3 What is wrong with the current surface

`pricing::surface` blends a short and a long realized window "MAX-leaning":
it lifts the weighted mean to at least 0.8 times the highest window "so a
vol spike in any single window raises the whole surface". The stated
reason is that buying vol too cheap after a spike is the expensive
mistake. That is a seller's logic. For a buyer, the expensive mistake is
buying vol too rich after a spike, when it mean-reverts, and the max-lean
does exactly that: it holds the bid up through the decay. Doc 07's −2.7
percent per turn is consistent with this bias. On top of that, a +5 vol
point `risk_premium` is added to realized before the base spread removes
4–6, so the effective bid sits at roughly trailing RV, with no margin.

The estimator study in §2.4 tests this directly. Expected outcome: drop
the max-lean, replace it with the post-shock regime's faster decay, and
move the risk premium to the quantile.

### 2.4 The study

Runs on the thin-slice backtester (§4, item G4). Two assets, two purposes.

**BTC, with true IV available.** Deribit DVOL hourly from 2021 in the lake.
The ablation doc 07 §3 proposed: the same bot priced at (a) trailing RV,
(b) the current surface, (c) the new forecaster at several quantiles, and
(d) true DVOL as the ceiling. Measures how much of the achievable vol P&L
each estimator captures and where each one's bias lives by regime.
Caveat from doc 07: a good BTC result says the estimator tracks market
IV; it does not say SUI's price is right, because on SUI we set the price.

**SUI, with no IV.** The operative question: at the price the estimator
would have bid, does the hedged book make money? Metrics per turn and
cumulative: realized minus paid variance, vol P&L, theta paid, gamma
scalp recouped, and the bias and quantile hit-rate of the forecast versus
what was realized over each option's life. Sliced by regime, tenor, and
moneyness. Includes 2025-10-10 in-sample and reports it separately.

Walk-forward discipline from doc 08 §9.2 applies: fit on the past, test
forward, never rank on the holdout.

Sweeps: `q_bid`, HAR weights, jump treatment on or off, sampling interval
(derived versus fixed 5-minute), post-shock decay rate, tenor.

**Later exploration: TimesFM 3 as a challenger forecaster.** Google's
zero-shot multivariate model (330M parameters, nine quantiles from the
10th to the 90th percentile, past covariates, cross-series attention;
weights on GitHub and Hugging Face) fits the estimator's interface: feed
it the daily forward-realized-variance series for SUI with BTC realized
vol, BTC DVOL, funding, volume, and jump share as past covariates, and
read its quantiles as `sigma_quantile`. It is not the estimator design.
HAR with the jump split stays the baseline and the interface. Conditions
for the exploration, after the baseline study has run:

- Same walk-forward folds, same QLIKE, bias, and quantile-calibration
  gates as §2.5.
- Scored only on data after the model's pretraining cutoff, with the
  leakage caveat labeled: public crypto series are likely in its corpus,
  so earlier folds cannot be trusted.
- Inputs are engineered aggregates at the derived per-asset sampling
  interval, not raw lake streams.
- Promote only if it beats HAR on calibration and loss out of sample; a
  match means the cheap model is enough. Promotion implies a pinned-weight
  sidecar or ONNX export so live and backtest call identical inference.

Deliverable: a doc `10-iv-estimator-findings.md` in the doc 07 style, with
the recommended default parameters and the bias each alternative carries.

### 2.5 Gates

- Forecast bias (mean of realized minus forecast over each option's life)
  is zero or slightly positive on every walk-forward fold for both assets.
- Quantile calibration: the 30th-percentile forecast is exceeded by
  realized vol about 70 percent of the time out of sample.
- Loss (QLIKE) beats trailing RV and beats the current surface on every
  fold.
- Post-shock regime: the forecast reverts faster than the 24-hour window
  after 2025-10-10, and the bid at that time is below what the current
  surface would have paid.
- Derived sampling interval reproduces doc 07 §4's signature table.
- Live and backtest call the same function and produce byte-identical
  output for the same history.
- No code path in the estimator or its inputs names an oracle provider.

---

## 3. Oracle-agnostic decision price

Doc 08 §1.6 and §6.1 assume Pyth. Restated:

- **Live.** The desk already reads prices from the oracle-service cache,
  which serves both providers under one key space. The desk's staleness
  and confidence gates stay provider-neutral: they take (price, confidence
  or spread proxy, publish time, receive time) and never inspect provider
  fields.
- **Historical.** The decision price is a proxy built from lake mids and
  degraded through a configured oracle model: update cadence, latency,
  confidence width, staleness. Every output is labeled `proxy_oracle=true`
  with the model's parameters in the manifest. The model is parameterized
  from observed live behavior of whichever provider is running, but the
  backtester never consumes provider history, so it cannot become
  dependent on one.
- **Not collected.** No Pyth Benchmarks history, no Switchboard history.
  This is now a scope rule, not a cost decision.
- **Retire** `/vol/realized` from every pricing consumer. It may stay as
  an operator convenience if nothing in the desk or backtester reads it.

---

## 4. Work plan

Ordered by value delivered per week. Each item names its gate. Items G1
through G6 precede doc 08's P1; they are the thin slice that produces the
first economic answers.

| # | Item | Gate |
|---|---|---|
| G1 | Apply the band correction (`band_pct_nav` 15, `band_wide_pct_nav` 25) in `hedge.rs` defaults and config. | Staging `/desk/state` shows the new bands; doc 07 §11 correction row closed. |
| G2 | PR D fix: expected hedge cost takes the current signed position and prices the incremental change; venue fee, fixed cost, and margin financing become config with non-zero staging values. | A put fill against a call-heavy book is charged near zero; the delta-net test uses a non-zero position. |
| G3 | Paper venue accrues funding against signed position at each settlement; `HedgeEvent` stream with `PartiallyFilled` and `Cancelled` handled by the rebalancer; open-order tracking. | Funding fixture reconciles to hand calculation for long and short; partial-fill and fill-after-cancel fixtures pass. |
| G4 | Thin-slice backtester `crates/backtester` v0: k-way merge over lake mids and Bluefin/Binance funding, oracle proxy model (§3), taker-only fills at configured spread, one net-delta book of calls and puts, signed hedge with bands, simple ledger (cash, options at model mark, perp P&L, funding, fees, gas). Reuses `pricing` and the estimator. Carries a minimal constant-flow injector (the capacity-mode subset of doc 08 §8: fixed accepted notional per day at a configured call/put mix and tenor) so the IV study and band sweep have positions to hedge; the full arrival and acceptance generator stays doc 08 PR N. Labels every run `proxy_oracle`, `taker_only`, `no_resale`, `constant_flow`. | Reproduces doc 07 §5 turnover and cost tables within tolerance; determinism gate; runs 2025-10-10 without freezing risk. |
| G5 | IV estimator crate `crates/vol-forecast` per §2.2, wired into `pricing::surface` behind the same `VolSurface` interface; live desk switched over. | §2.5 gates. |
| G6 | IV study per §2.4 and findings doc 10. | Recommended defaults adopted in config with the study's bias numbers attached. |
| G7 | Cost lines: protocol `fee_bps` on gross premium in the bid and the ledger; curator performance fee and protocol cut in attribution; hurdle redefined as depositor net in doc 08 §0.4. | A run with `fee_bps` 50 and a 20 percent curator fee shows both lines and the net hurdle test. |
| G8 | PR E: put exercise policy and three PTB paths per doc 08 §4.4. | Atomic path, fallback, and failure tests; no stranded asset. |
| G9 | PR G: durable reservations keyed by request id with explicit transitions, `CapitalSnapshot`, and `CapitalPolicy` per doc 08 §4.6. | Restart during live quotes preserves capacity; freshness gates block on stale NAV. |
| G10 | PR F completion: rally and crash loss on hedges before monetization, settlement cash and underlying required per expiry, gamma by expiry, flash utilization; soft throttles and hard solvency declines wired into the quote path. | Call-heavy, put-heavy, and mixed fixtures trip the intended throttles. |
| G11 | Data room: buy-base router ladder (S1c) with `direction` column; Bluefin funding-rate-history poller plus settlement derivation from ticker rollovers; chain inclusion and detection latency capture; S5 silver. | Each stream visible in the stall metrics; S5 determinism test green. |
| G12 | Runtime-path tests for merged work: trader-side decline, legacy inventory block, rebalancer end-to-end for long and short targets, one-row-per-RFQ funnel including auctions, recorder flag default off. | Tests exist and run in CI. |
| G13 | Flow generator reuses the live `/buckets` lattice so synthetic writers request only displayable specs. | Generated specs round-trip through `bucket_registry::key`. |

After G13, doc 08's P1 through P5 proceed as written, with two changes:
the parity kernel and exact ledger (P1) are the final validation gate the
thin slice must reconcile against, not the first milestone; and P4's
market mode is labeled scenario-only until mainnet RFQ capture exists.

```text
G1 G2 G3 ──┐
           ├─▶ G4 thin slice ─▶ G5 estimator ─▶ G6 study ─▶ G7 costs
G11 data ──┘                                        │
G8 G9 G10 G12 (production correctness, parallel)    ▼
                                    doc 08 P1 kernel + ledger (validation)
                                                    ▼
                                    P2 replay · P3 execution · P4 flow · P5 sweeps
```

Tickets to raise: one per G-item, parented to the backtesting epic, with
G1 and G2 first since both are live-wrong on staging.

---

## 5. What this still cannot claim

Unchanged from doc 08 §0.2 and §12. Writer arrival and acceptance stay
stated priors until mainnet capture exists. The framework reports
survival, capacity, and break-even surfaces conditional on those priors,
plus a bid-discipline result that is real because it is measured against
realized history. It does not report an APY.
