# 06 — P2: `test-runner`

API-first simulation service (spec D12/D13). New crate `rust-backend/services/test-runner`, port **9021**, nginx-routed (dashboard + agents call it; BYO API keys at P4). Fork the skeleton from **`rust-backend/tools/backtester`** (scenario/sweep/report/rayon structure — `scenario.rs`, `run.rs`, `report.rs` transfer directly) and the engine ideas from **`crates/vault-sim`** (both removed in SO-452 — recover from git history, or start from the new `crates/backtester`); the covered-call-vault specifics get replaced with the studio strategy model.

```
services/test-runner/
  src/{main,lib,config,router,state}.rs
  src/handlers/{mod,tests,reports}.rs
  src/engine/{mod.rs,t1_replay.rs,t2_live.rs,fills.rs,portfolio.rs,metrics.rs}
  src/data/{mod.rs,spot_history.rs,import.rs}
  src/strategy_host.rs          runs the Python strategy against the engine (see §4)
  src/db/…
```

## 1. API (spec §8.2, unchanged)

```
POST /v1/tests        { specId, specVersion, specBody, mode: "t1"|"t2", window: "standard"|{fromMs,toMs}, seed }
                      → { jobId }        (specBody is the full snapshot; hash-checked against provisioner)
GET  /v1/tests/:id    → { status: queued|running|done|failed, reportId? }
GET  /v1/reports/:id  → immutable report (below)
```

Jobs run on a bounded worker pool (rayon like the backtester; N concurrent jobs, queue beyond). Metering: per-user daily run counts in `test_meter` (same plumbing later covers BYO API keys, spec §17).

## 2. Data plane (the real work of P2)

**T1 needs SUI (and later BTC/WAL) spot history. Today the repo has none usable on staging** (00 §delta-5): price-charting's Tiger tsdb is paused, and it only ever held staging-era bars anyway — months of multi-regime history (spec: chop/crash/rally windows + holdout) never existed on-chain here.

Plan:

1. **Own table, own import.** `spot_history (asset, ts, open, high, low, close, volume)` in `test_runner_<env>`, hypertable-free plain Postgres (1m candles × a few years × 3 assets is small — ~1.6M rows/asset at 1m over 3 years). One-off `src/data/import.rs` CLI subcommand (`test-runner import --asset SUI --csv …`) ingesting exchange-exported candles — Binance's public historical dumps (`data.binance.vision`, free full-history 1m candles for SUIUSDT since the May 2023 listing) are the pragmatic source; Coinbase/OKX public candle APIs as cross-checks. Provenance recorded in an `imports` table. The multi-regime standard eval and the holdout window are **fixed, versioned window definitions in config** — e.g. `windows.toml` naming `chop-2025Q1`, `crash-2025-08`, `rally-2026Q2`, `holdout-2026Q3` with absolute ms ranges — so every report is comparable and the holdout is enforceable (the API rejects explicit windows overlapping the holdout unless `final: true`, which only the dashboard's pre-deploy step sets).
2. **Optionally resume Tiger + price-charting** (ops task, independent): un-comment the compose block, resume the instance, and the exchange watcher starts collecting *live* bars — useful for T2 analysis and future recency, not a substitute for deep history.
3. Realized vol for option marking: compute from `spot_history` directly (don't lean on oracle-service `/vol/realized`, which is live-only).

## 3. T1 engine (`t1_replay.rs`)

Deterministic, seeded (`StdRng::seed_from_u64(seed)`; **all** randomness flows from it — spec D14):

- Clock: replay `spot_history` at candle resolution; strategy `on_tick` cadence from the spec's `refresh_s`, driven by sim time.
- Derivative layer: synthetic option chain per spec market — strikes gridded around spot, tenor from spec; marked Black-Scholes with vol = rolling realized vol from the replayed path (window per config) plus a fixed vol-risk-premium knob.
- Flow model: taker arrivals as a Poisson process (intensity calibrated per market from `exchange_fills` counts on staging; config fallback), arrival side/size sampled seeded; a bot maker order fills when an arrival crosses it at its price (`fills.rs`), plus immediate fill for taker-style intents at mark ± spread cost.
- Portfolio (`portfolio.rs`): positions, cash, greeks (reuse the pricing math already in the backtester/`crates/vault-sim` lineage), mark-to-market P&L series, drawdown, fees at the market's `fee_bps`.
- The guard is simulated too: band + notional checks mirror `TradePolicy` semantics so a spec that would trip the on-chain limiter **fails its backtest visibly** (`guardRejections` in the report) instead of surprising in production.

## 4. Hosting the Python strategy

The strategy under test is the same Python that will run live (spec: reports pin what deploys). Run it, don't reimplement it: the engine exposes the **gateway wire protocol in-process** — spawn `python -m curator_sdk.simhost` as a subprocess; it runs the real `runtime.py` loop with `GatewayClient` pointed at a unix-socket/localhost stub served by the engine (`strategy_host.rs`), which answers `book/price/vault/place` from sim state and advances sim time per tick. Determinism holds because all stochasticity lives engine-side. This also means T1 exercises the SDK itself — SDK regressions fail tests before they fail bots.

(Simpler fallback if subprocess orchestration proves painful in P2: reimplement the four v1 primitives in Rust inside the engine, keyed by primitive name — acceptable *only* while strategies are primitives; the bespoke path (P4) requires the subprocess host. Build the host.)

## 5. T2 (`t2_live.rs`)

Thin by design: provision a **paper bot** against staging — the gateway's paper engine (a `mode=paper` flag on the bot row: intents are simulated against the *live* staging book instead of signed/submitted; fills modeled by book-crossing) — and let it run for the window; the report aggregates the same metrics from the paper portfolio. market-sim provides the flow; note its staging cadence is 12h re-quotes (`spot_interval_secs = 43200`) — for useful T2 sessions add a faster studio profile to market-sim config (e.g. 60s) behind a config key, or accept T2 as a smoke test rather than a statistics source in v1.

## 6. Report artifact

`reports` row, immutable, `(id, spec_id, spec_version, spec_hash, spec_body JSONB, mode, window, seed, engine_version, created_at, body JSONB)` where `body`:

```json
{
  "pnlSeries": [[ts, nav], …],
  "maxDrawdownPct": 18.2,
  "returnPct": 6.4, "sharpe": 0.9,
  "fills": { "count": 214, "notional": "…", "byMarket": {…} },
  "greeks": { "deltaSeries": […], "vegaAvg": … },
  "guardRejections": { "band": 0, "notional": 2 },
  "windows": { "chop-2025Q1": {…}, "crash-2025-08": {…}, "holdout": null | {…} },
  "summary": "human-readable paragraph"
}
```

`engine_version` (crate version + windows.toml hash) makes reports reproducible-or-explainably-not. The dashboard renders `body`; the agent fetches the same JSON (spec: no lossy pasting).

## 7. DB schema (`test_runner_<env>`)

```
jobs         (id, spec_id, spec_version, spec_hash, mode, window, seed, status, error, report_id, requested_by, created_at)
reports      (§6)
spot_history (asset, ts) PK — candles
imports      (id, asset, source, range, row_count, created_at)
test_meter   (principal, day, runs)
```

## 8. Alert ids

`test-runner-job-failed` (post-retry), `test-runner-data-gap` (a requested standard window missing candles — misconfigured import).
