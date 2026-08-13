# Data Room — Design Specification

A generalized market-data lake for backtesting market-making and trading strategies: exchange-agnostic collectors, an immutable Parquet archive on S3 organized bronze → silver → gold, and DuckDB as the query surface. First feed: BTC spot on Coinbase (Binance is geo-blocked from our infra; see §6.5 on the USDC/USD nuance). First derived product: realized volatility at the finest defensible sampling interval.

- **Status:** Draft v1.2 — open questions resolved; deployment/integration/rollout added (§10)
- **Date:** 2026-08-13
- **Depends on:** nothing in the live stack — deliberately decoupled from the services deploy set

---

## 1. Overview

The data room is a standalone pipeline with four stages:

```
exchange websockets
      │
      ▼
┌─────────────┐    JSONL.gz     ┌──────────────┐   Parquet    ┌────────────┐
│  collector   │ ──────────────▶ │  normalizer  │ ───────────▶ │ gold jobs  │
│ (long-lived) │    BRONZE       │   (batch)    │   SILVER     │  (batch)   │
└─────────────┘                 └──────────────┘              └────────────┘
                                                                    │ Parquet
                                                                    ▼ GOLD
                              DuckDB / Polars / notebooks ◀── s3://<bucket>/
```

- **Collector** — long-lived Rust binary per exchange connection group. Subscribes to websocket streams, writes raw messages verbatim to local spool files, rotates hourly, uploads to S3 bronze. Zero parsing beyond framing.
- **vision-sync** — small backfill tool (§6.6) that mirrors Binance's public flat-file dumps (`data.binance.vision`) into bronze, checksum-verified. Historical tick depth without live Binance access (which is geo-blocked; the dump bucket is not — verified 2026-08-12).
- **Normalizer** — batch job. Reads bronze partitions, parses exchange-native payloads through per-exchange adapters into canonical event tables, writes silver Parquet. Idempotent per partition.
- **Gold jobs** — batch analytics over silver: bars, realized-vol series, consolidated cross-venue prices, data-quality/gap ledgers. Cheap to delete and regenerate.
- **Query surface** — a checked-in DuckDB catalog (`catalog.sql`) defining views over the S3 globs. No query server.

Guiding rules, in priority order:

1. **Bronze is sacred.** Raw bytes, append-only, never rewritten. Every downstream layer must be regenerable from bronze alone.
2. **Normalize by event type, never by exchange.** One `trades` table for every venue; adding an exchange means a new adapter and new partition directories, never a schema change.
3. **Batch over streaming.** Backtesting tolerates hours of latency. No Kafka, no stream processors, no query servers. The only long-lived process is the collector.
4. **Files over services.** The ops surface is one small binary, S3, and cron. Nothing here can page anyone about the production protocol.

## 2. Goals & non-goals

### Goals

- Capture SUI spot (Binance) tick data with documented gap accounting, durably archived in bronze.
- Canonical silver schemas that already fit the target breadth — spot trades, top-of-book quotes, perps funding, options quotes — so later feeds are adapters, not redesigns.
- A gold realized-vol series over the first asset, computed at multiple sampling intervals with a volatility-signature analysis to pick the finest defensible interval.
- Everything queryable from DuckDB (CLI, Rust, or a Python notebook) with one `IMPORT`/`ATTACH`.

### Non-goals

- **Not a live data plane.** The mm-bot, oracle-service, and price-charting keep their own feeds. Nothing in production reads the data room (a later strategy may consume gold *outputs*, but never with a liveness dependency).
- **No order-book depth (L2) in v1.** Schemas reserve space for it (§13, P4); collectors don't capture it yet.
- **No UI.** Notebooks and DuckDB are the interface.
- **No general job orchestrator.** Cron + idempotent batch jobs until that demonstrably breaks.

## 3. Repository & runtime layout

New top-level directory, its own Cargo workspace, **not** part of `rust-backend/services` or the deploy/force lists:

```
data-room/
  Cargo.toml               # workspace
  collector/               # binary: ws → bronze spool → S3
  vision-sync/             # binary: Binance flat-file dumps → bronze (§6.6)
  normalizer/              # binary: bronze → silver
  gold/                    # binary (subcommands): bars, rv, gaps
  adapters/                # crate: per-exchange parse/normalize (binance, ...)
  schema/                  # crate: canonical Arrow schemas + conventions (single source of truth)
  catalog/catalog.sql      # DuckDB views over S3
  notebooks/               # research; signature-plot notebook lives here
  docs/                    # runbook, adapter-authoring guide
```

Runtime: a dedicated `data-room-host` EC2 instance running the collector under compose with restart-always, plus timer-driven batch jobs — full deployment/integration detail in §10. S3 bucket `<org>-data-room` with zone prefixes. The host is stateless apart from the spool directory; losing it loses at most the unuploaded spool window (§6.4).

Infra is defined in the existing `rust-backend/infra` Terraform root (new bucket + lifecycle rules, an instance-profile role scoped to the bucket, and the collector host). Standing gotchas apply: that root's state is **local** to the canonical `options-2` worktree — plan before apply, and never apply from another checkout.

## 4. Storage layout

```
s3://<bucket>/
  bronze/v1/
    exchange=coinbase/stream=matches.BTC-USD/date=2026-08-12/hour=13/
      <boot_id>-<file_seq>.jsonl.gz
    exchange=coinbase/stream=ticker.BTC-USD/date=2026-08-12/hour=13/...
    exchange=binance/source=vision/market=spot/kind=trades/symbol=BTCUSDC/
      BTCUSDC-trades-2019-05.zip            # dump files verbatim, monthly + daily
    exchange=binance/source=vision/market=spot/kind=aggTrades/symbol=BTCUSDC/...
  silver/v1/
    trades/exchange=coinbase/symbol=BTC-USD/date=2026-08-12/part-00.parquet
    book_top/exchange=coinbase/symbol=BTC-USD/date=2026-08-12/part-00.parquet
    funding_rates/exchange=.../symbol=.../date=.../...        (P2)
    options_quotes/exchange=deribit/underlying=BTC/date=.../...  (P3)
    instruments/snapshot_date=2026-08-12/instruments.parquet
  gold/v1/
    bars/freq=1s/exchange=coinbase/symbol=BTC-USD/date=.../...
    rv/symbol=BTC-USD/date=.../...
    gaps/exchange=coinbase/date=.../...
```

Conventions:

- **Hive-style partition keys** (`key=value`) so DuckDB/Polars/Spark all prune for free.
- **Zone-level schema version** (`v1/`) — a breaking schema change writes a new version prefix populated by replaying bronze; readers migrate by repointing the catalog. No in-place migrations, no per-row version columns.
- **Dates and hours are UTC** everywhere, derived from `ts_recv`.
- **File sizing:** target 100 MB–1 GB per Parquet file; a partition below ~16 MB after a day is fine as one small file (SUI trades will be — don't fight it). Options partition by `underlying`, never by contract, to avoid the small-files problem.
- **S3 lifecycle:** bronze → Infrequent Access at 30 days, Glacier Instant Retrieval at 180; silver/gold stay standard (they're small and hot). Nothing auto-expires.
- Bucket versioning ON for `silver/` and `gold/` (cheap insurance against a bad batch job); OFF for `bronze/` (append-only by construction; objects are never overwritten).

## 5. Schemas

Canonical Arrow/Parquet schemas live in the `schema` crate; this section is the contract. Column conventions across all tables:

- `ts_event` — **int64, nanoseconds UTC** — the exchange's timestamp. Nullable where the venue doesn't provide one.
- `ts_recv` — int64 ns UTC — when *our* collector observed the message. Never null for live-captured rows; **null for bulk-archive-sourced rows** (§6.6), which is the honest marker that we never observed them in real time. **Backtest fills must reference `ts_recv`** where present; using `ts_event` as the actionable time manufactures latency-free alpha, and backtests over archive-only history must model latency explicitly.
- `exchange`, `instrument_id` — dictionary-encoded strings. `instrument_id` is ours (`sui-usdt.binance`), never the venue-native symbol.
- Prices and sizes are **float64** in normalized human units (price in quote per base, size in base). Rationale: this is a research store, not an accounting ledger; f64 round-trips every real-world tick exactly enough, and every downstream tool speaks it natively. Bronze retains the exact original strings if fixed-point is ever needed.
- Rows within a file are sorted by `ts_recv`; Parquet row-group stats then make time-range reads cheap.

### 5.1 `silver/trades`

| column | type | notes |
|---|---|---|
| `ts_event` | i64 ns | exchange trade time |
| `ts_recv` | i64 ns | |
| `exchange` | dict str | |
| `instrument_id` | dict str | |
| `price` | f64 | |
| `size` | f64 | base units |
| `side` | dict str | aggressor: `buy` / `sell` / null |
| `trade_id` | str | venue-native; dedup key with (`exchange`,`instrument_id`) |
| `src_file` | dict str | bronze object key (lineage/debug) |
| `src_line` | i32 | line within bronze file |

### 5.2 `silver/book_top` (best bid/offer)

| column | type | notes |
|---|---|---|
| `ts_event` | i64 ns, nullable | Binance spot `bookTicker` carries none — null |
| `ts_recv` | i64 ns | |
| `exchange`, `instrument_id` | dict str | |
| `update_id` | i64 | venue sequence; dedup + ordering key |
| `bid_px`, `bid_sz`, `ask_px`, `ask_sz` | f64 | |
| `src_file`, `src_line` | | lineage |

### 5.3 `silver/funding_rates` (P2)

`ts_event`, `ts_recv`, `exchange`, `instrument_id`, `rate` (f64, per-interval as quoted), `interval_hours` (f32 — 8 Binance, 1 Hyperliquid), `kind` (`predicted` | `settled`), `mark_price`, `index_price` (f64, nullable), lineage columns.

### 5.4 `silver/options_quotes` (P3)

`ts_event`, `ts_recv`, `exchange`, `instrument_id` (one per contract), `bid`, `ask`, `bid_sz`, `ask_sz`, `mark_price`, `mark_iv`, `underlying_price`, `open_interest` (all f64, nullable per venue), lineage columns. Strike/expiry/type live in `instruments`, joined on `instrument_id`.

### 5.5 `silver/instruments`

Full-snapshot table, rewritten per `snapshot_date` (slowly changing; readers take the latest snapshot ≤ their as-of date):

| column | type | notes |
|---|---|---|
| `instrument_id` | str | `<base>-<quote>[.-modifier].<exchange>`, lowercase |
| `exchange` | str | |
| `native_symbol` | str | exactly what the venue API uses |
| `asset_class` | str | `spot` \| `perp` \| `option` \| `future` |
| `base`, `quote` | str | |
| `tick_size`, `lot_size`, `contract_multiplier` | f64 | |
| `strike` | f64, nullable | options |
| `expiry` | i64 ns, nullable | options/futures |
| `opt_type` | str, nullable | `call` \| `put` |
| `funding_interval_hours` | f32, nullable | perps |
| `listed_at`, `delisted_at` | i64 ns, nullable | |

Populated by a small per-exchange instrument fetcher (REST), run daily by cron. For v1 this is one Binance row; the table exists from day one because it is the generalization seam.

### 5.6 Bronze envelope

One gzipped JSONL file per (exchange, stream, hour, collector boot). Each line:

```json
{"ts_recv_ns": 1765540123456789012, "seq": 41872, "payload": "<raw message text, verbatim>"}
```

`seq` is the collector's per-connection monotonic counter (gap forensics). The payload is the untouched websocket frame — no parsing, no re-serialization. File-naming: `<boot_id>-<file_seq>.jsonl.gz` where `boot_id` is a random id per collector start, so a restart can never overwrite a predecessor's object.

### 5.7 Gold tables

**`gold/bars`** — OHLCV + mid at fixed frequencies (`1s`, `1m`, `1h` initially), from trades and/or `book_top` mid. Columns: `ts_open`, `freq_s`, `exchange`, `instrument_id`, `open/high/low/close`, `volume`, `n_trades`, `mid_close`, `source` (`trades`|`mid`).

**`gold/rv`** — the realized-vol product (§8): `ts` (window end), `exchange`, `instrument_id`, `window_s`, `sample_interval_s`, `source` (`trades`|`mid`), `estimator` (`close_close` | `rv_subsampled`), `sigma_ann` (f64), `n_returns`, `coverage` (fraction of window actually covered by data — see gaps).

**`gold/gaps`** — the honesty ledger: one row per detected capture gap: `exchange`, `stream`, `gap_start_ns`, `gap_end_ns`, `kind` (`disconnect` | `venue_outage` | `spool_loss`), `detected_by`. Every backtest and every RV window must be maskable against this table; `rv.coverage` is derived from it.

## 6. Collector specification

One binary, config-driven (`collector.toml`): a list of (exchange, streams[]) connection groups. v1 config: Coinbase Exchange websocket feed, `matches` + `ticker` + `heartbeat` channels for `BTC-USD` (see §6.5).

### 6.1 Behavior

- Maintain one websocket per connection group; respond to venue pings; reconnect with jittered exponential backoff (cap 30 s).
- On every message: stamp `ts_recv_ns`, increment `seq`, append the JSONL line to the current spool file. No parsing on the hot path.
- Rotate spool files hourly on the UTC boundary (and at 512 MB as a safety cap); on rotation, gzip and enqueue upload to the bronze prefix; delete local file only after a verified S3 put (ETag check).
- On startup and after every reconnect, emit a synthetic bronze line `{"kind":"collector_marker","event":"connect"|"disconnect", ...}` so gap detection has explicit boundaries even when the venue was quiet.

### 6.2 Gap semantics

The collector does not try to backfill. A disconnect is recorded (markers + missing `seq` continuity), the gold `gaps` job turns it into ledger rows, and analyses mask it. Optional REST backfill of trades into a separate `bronze/.../source=rest_backfill/` prefix is a P2-listed nice-to-have, kept apart so websocket-only lineage stays clean.

### 6.3 Observability

Prometheus metrics on a local port: messages/s per stream, spool lag, last-upload age, reconnect count. Two alert conditions, wired into the existing alerting stack with the repo's `alert_id` convention: `dataroom-collector-stalled` (no message on a subscribed stream for > 5 min) and `dataroom-upload-stalled` (oldest unuploaded spool file > 3 h).

### 6.4 Durability budget

Spool is local disk; a host loss forfeits at most the current unuploaded window (≤ 1 h + upload lag). Accepted for v1 — it becomes a `gaps` row like any disconnect. If that ever matters, shrink rotation to 5 min; do not add infra.

### 6.5 Coinbase specifics (P0 venue)

- **Feed:** Coinbase Exchange websocket, `wss://ws-feed.exchange.coinbase.com`. The market-data channels we need are public — no account, API key, or contract required. (Verify at build time against current docs; Coinbase has moved auth boundaries before — notably full `level2` became auth-required in 2022.)
- **Channels:** `matches` (every trade → silver `trades`), `ticker` (fires per match with `best_bid`/`best_ask` → silver `book_top`), `heartbeat` (1 s liveness + sequence checkpoints). `ticker`'s BBO updates are trade-triggered, so the mid-quote series moves at trade cadence, not quote cadence — fine for P0 RV; a true quote-cadence BBO needs a `level2`-family channel (auth'd API key or the batched variant) and is bundled with the P4 depth work.
- **Gap detection:** Exchange messages carry per-product `sequence` numbers — the adapter checks continuity and emits `gaps` evidence on skips, which is stronger than the wall-clock-only heuristics available on some venues.
- **Instruments:** public REST `GET /products` feeds the instrument fetcher.
- **The USDC nuance:** the P0 target is "BTC vs USDC," but Coinbase does not run a separate BTC-USDC book — USDC and USD are treated 1:1 on-platform and the deep book is `BTC-USD`. We therefore record the instrument honestly as `btc-usd.coinbase` (quote `USD`) and treat it as the BTCUSDC price series. The literal USDC-quoted book is Binance `BTCUSDC`, whose full history arrives via `vision-sync` (§6.6) even though its live feed is geo-blocked.

### 6.6 Bulk backfill tool: `vision-sync`

Binance publishes free bulk dumps at `data.binance.vision` — a public CloudFront/S3 bucket, **not** geo-blocked (verified from a US residential IP the same day `api.binance.com` returned 451). Monthly + daily CSV zips per symbol for `trades`, `aggTrades`, and `klines`, back to each symbol's listing, each with a `.CHECKSUM` (sha256) sidecar. Full BTCUSDC history measured at 87 monthly files, ~4.8 GB zipped for trades+aggTrades — minutes of transfer, versus days of rate-limited REST crawling for equivalent depth on Coinbase.

The tool is a one-shot/cron mirror, config-driven (`vision.toml`: market, symbols[], kinds[], optional date range):

- **List → diff → fetch.** Enumerate the vision bucket prefix (S3 `list-type=2`), diff against our bronze prefix, download only missing files. Naturally idempotent and resumable; a monthly cron picks up each newly published month (and daily files for the seam before live capture began, if wanted).
- **Verify then upload verbatim.** Check the sha256 sidecar, then put the **original zip untouched** into `bronze/v1/exchange=binance/source=vision/...`. Bronze-is-sacred applies: no unzipping, no re-compression, no CSV parsing at ingest time. The normalizer's `binance_vision` adapter reads CSVs straight out of the archived zips.
- **Concurrency:** a handful of parallel downloads saturates the host NIC; nothing fancier warranted.
- **Normalization rule — no double counting:** `trades` and `aggTrades` describe the *same* fills (aggTrades merges same-taker/same-price fills). Both kinds are archived in bronze, but only `kind=trades` normalizes into silver `trades`; aggTrades stays archive-only unless a consumer specifically wants the aggregated view (then it gets its own silver table, never merged rows).
- **No seam-stitching in silver:** Binance history (`btc-usdc.binance`) and Coinbase live capture (`btc-usd.coinbase`) are different instruments and remain separate series; any cross-venue continuity or splicing is a gold-layer decision, made explicitly.
- `ts_event` in dumps is millisecond (older files) or microsecond (newer) epoch — the adapter normalizes to ns; `ts_recv` is null for dump-sourced rows (we didn't observe them live), which is also the honest marker distinguishing backfilled from captured data.

## 7. Normalizer specification

- **Unit of work: one (stream, UTC day) partition.** Cron runs shortly after each UTC day closes (plus a catch-up scan for late bronze uploads); a `--date`/`--stream` CLI reprocesses anything on demand.
- Reads all bronze files for the partition, parses via the exchange adapter, dedups (venue key: `trade_id` / `update_id`), sorts by `ts_recv`, writes silver Parquet (zstd) to a temp key, then renames over the partition. **Overwrite-partition idempotency:** re-running a day replaces that day exactly; never appends.
- Malformed lines: logged, counted, written to `silver/v1/_rejects/` with the same partitioning, never silently dropped. > 0.1 % rejects on a partition fails the run loudly.
- **Determinism is a hard requirement:** same bronze in, byte-identical silver out (fixed row order, fixed writer settings). This is what makes "replay bronze after an adapter bugfix" trustworthy, and it's CI-testable.

### 7.1 Adapter contract

An adapter (per exchange) implements: `streams_for(instrument) -> [stream names]`, `parse(stream, payload) -> Vec<CanonicalEvent>`, and `fetch_instruments() -> Vec<Instrument>` (REST). `CanonicalEvent` is the closed enum in the `schema` crate (`Trade`, `BookTop`, `FundingRate`, `OptionQuote`, …). Adding a venue = one adapter crate + golden-file tests (§11); the normalizer, schemas, and layout are untouched.

## 8. Gold: realized volatility

The deliverable behind "most granular interval possible": tick data can be sampled arbitrarily finely, but below some interval, microstructure noise (bid-ask bounce) inflates RV — naive tick-level RV is *wrong*, not just noisy. The job therefore computes RV across a grid and the notebook picks the operating point empirically.

- **Inputs:** silver `trades` (trade-price returns) and `book_top` (mid-quote returns — bounce-free, usually the better series).
- **Sampling grid:** {1 s, 5 s, 15 s, 60 s, 300 s} × windows {1 h, 24 h, 168 h} × both sources.
- **Estimators:** `close_close` — last-observation-carried-forward onto the grid, zero-mean Σr², annualized by realized span (same convention as the mm-bot's `RollingVolBuffer`, deliberately, so numbers are comparable); `rv_subsampled` — averaged over K offset grids (Zhang-style two-scale lite), the cheap noise-robust upgrade.
- **Gap handling:** windows intersecting `gaps` rows get `coverage < 1`; returns are never computed across a gap boundary. Consumers filter on `coverage`.
- **Acceptance analysis (one notebook, committed):** the volatility-signature plot — RV vs sampling interval for both sources. Where the curve flattens is the finest defensible interval; that becomes the documented default (`rv.sample_interval_s` recommendation) and directly answers whether the mm-bot's current 300 s / oracle-mid choice is leaving information on the table.

## 9. Query surface

`catalog/catalog.sql`, checked in, is the single entry point:

```sql
CREATE OR REPLACE VIEW trades AS
  SELECT * FROM read_parquet('s3://<bucket>/silver/v1/trades/**/*.parquet', hive_partitioning=1);
-- ... one view per table; secrets via DuckDB s3 credential chain (env / instance role)
```

Usage contract: notebooks and ad-hoc analysis run DuckDB (or Polars) directly against S3 — partition pruning makes single-day/single-symbol queries interactive from a laptop. A `just sync` helper may mirror a partition subset locally for offline work; nothing depends on it.

## 10. Deployment, infrastructure & rollout

### 10.1 Position in the microservice architecture

The data room is a **peer stack, not a peer service**. It shares the account, the Terraform root, ECR, CI, and observability conventions with the protocol services — and nothing else:

- **Zero runtime dependencies on the protocol stack.** No token-info, no RDS, no gas-station, no deployments.json, no service-to-service HTTP. Its only interfaces outward are the S3 bucket (data), Prometheus/Loki (telemetry), and GitHub Actions (build).
- **Zero protocol dependencies on it.** Nothing in staging/prod reads the data room in v1. The confirmed eventual consumer (mm-bot cold-start vol seeding, §14.4) will be a *pull with graceful degradation* — bot falls back to `fallback_vol` exactly as it does today if gold is stale or absent — never a liveness dependency.
- **Explicitly outside the deploy set.** Not in either host's compose file, not in the Deploy workflows' force lists, not health-check-gated with protocol services. The existing deploy pipeline rolls back the *whole planned set* on one failing health check; the data room must never be able to trigger (or be rolled back by) a protocol deploy, and vice versa.
- **Own host, deliberately.** `data-room-host` (t3.small, 100 GB gp3). Isolation is the point: backfill unzips and spool churn are exactly the disk-pressure profile that has wedged `options-host` before (containerd snapshot fill → SSM/cloud-init dead). Disk-usage alarm from day one this time.

### 10.2 Infrastructure (Terraform, `rust-backend/infra`)

New resources in the existing root (local state — plan/apply only from the canonical `options-2` worktree):

| Resource | Notes |
|---|---|
| S3 bucket `<org>-data-room` | lifecycle rules per §4; versioning on `silver/`+`gold/` prefixes only |
| IAM instance profile | RW scoped to the bucket, nothing else; collector host assumes it (no static keys) |
| ECR repos ×2 | `data-room-collector` (long-lived daemon image), `data-room-batch` (normalizer + vision-sync + gold + instruments fetcher as subcommands in one image — one repo instead of four; new ECR repo *must* land in `ecr.tf` before first push or CI 403s) |
| `data-room-host` EC2 | t3.small, 100 GB gp3, SSM-managed like the other hosts; minimal cloud-init (docker + SSM agent) — deliberately **not** wired into the gatus/user-data machinery, since editing that bounces the protocol hosts |
| SG | egress-only, plus Prometheus scrape ingress from the monitoring host |
| CloudWatch disk alarm | ≥ 80 % root volume — the options-host lesson, encoded |

### 10.3 Build & deploy pipeline

- **CI:** the `data-room/` workspace builds and tests in GitHub Actions alongside the repo (fmt, clippy, unit + golden-file + determinism tests). On merge to `staging`, both images build and push to ECR tagged by commit SHA.
- **Deploy:** a separate small workflow, `Deploy data-room` — SSM RunCommand to the host: update the image tag in the compose `.env`, `docker compose pull && up -d` (collector), done. Batch jobs pick up the new tag on their next timer firing. It is **not** part of Deploy staging / Deploy prod and has no health-check-gated rollback choreography — if the collector comes up sick, the alerts (§10.4) say so and redeploying the previous tag is the whole rollback.
- **Scheduling on-host:** systemd timers (not container-internal cron) invoking `docker compose run --rm batch <subcommand>`: `normalizer` daily 00:30 UTC + catch-up scan, `instruments` daily, `vision-sync` monthly (+ daily during the pre-live-capture seam), `gold` daily after normalizer.
- **No staging/prod split.** One environment; the bucket *is* the product. This is safe because of the layering: a bad deploy cannot corrupt bronze (append-only, verbatim, never rewritten), and silver/gold are versioned and regenerable from bronze. "Prod incident" degrades to "capture gap," which is a first-class, ledgered concept (§8) rather than an outage.

### 10.4 Observability

Same conventions as the rest of the stack, same Grafana:

- Collector `/metrics` scraped by the existing Prometheus (messages/s per stream, spool lag, last-upload age, reconnects); batch jobs emit per-run summary metrics via pushgateway-style one-shots or structured logs.
- Logs ship to Loki; alert conditions use the repo's `alert_id` convention: `dataroom-collector-stalled`, `dataroom-upload-stalled` (§6.3), `dataroom-quality-gate` (§11), plus the host disk alarm (§10.2).
- Gatus/status-page entry: skipped in v1 (gatus config rides protocol-host cloud-init user_data; not worth bouncing hosts for an internal research stack). Revisit if the data room ever gains external consumers.

### 10.5 Rollout plan

Sequenced so every step has a verification gate before the next, mapping onto phases P0/P1 (§12):

| Step | Action | Gate before proceeding |
|---|---|---|
| **R0 — infra** | Terraform: bucket, IAM, ECR, host, SG, alarm. Plan reviewed, applied from canonical worktree | `aws s3 ls` from the host via instance profile works; ECR push from CI succeeds |
| **R1 — collector soak** | Deploy collector (Coinbase BTC-USD). Let it run ≥ 48 h | Bronze objects landing hourly; metrics in Grafana; **induced-failure drill**: kill the websocket once, confirm reconnect + marker rows + `dataroom-collector-stalled` fires and resolves |
| **R2 — normalizer** | Enable normalizer + instruments timers | 7 consecutive days: silver partitions present, quality gate green, gaps ledger reconciles to 100 % of wall-clock, determinism test green in CI (= P0 acceptance) |
| **R3 — backfill** | Run `vision-sync` full BTCUSDC pull (one-shot), normalize history, enable monthly timer | Row counts vs dump-file line counts reconcile; 2019→present queryable via `catalog.sql`; spot-check months against Binance's published klines |
| **R4 — gold** | Enable gold timer; run signature-plot notebook | P1 acceptance: RV grid over live + historical data; documented recommended sampling interval; cross-venue comparison note |
| **R5 — steady state** | Write runbook, hand off to routine ops | Runbook covers: restart, replay-a-day, add-an-exchange checklist, gap post-mortem checklist |

Rollback at any step is trivial by construction: stop the container / disable the timer; data already written is immutable (bronze) or regenerable (silver/gold). Nothing downstream is coupled, so there is no coordination with protocol deploys at any point.

### 10.6 Cost

SUI/BTC-scale live capture is a few MB/day of bronze; the full Binance backfill is ~5 GB once. S3 + t3.small + 100 GB gp3 ≈ **$25–35/month**. Options chains (P3) are the first real storage mover (~1–5 GB/day bronze for a full Deribit BTC chain ticker) — still trivial for S3; revisit lifecycle tiers then.

## 11. Testing & data quality

- **Golden-file adapter tests:** captured real bronze samples per venue in-repo; adapters must produce exact expected canonical rows. This is the regression net for venue format drift.
- **Determinism test (CI):** normalize a fixture partition twice → byte-identical Parquet.
- **Daily quality gate (end of gold cron):** per partition — dedup effectiveness, `ts_recv` monotonicity, price sanity (no zero/negative, > 50 % single-tick jumps flagged), gap accounting reconciles marker rows vs `gaps` ledger, reject-rate threshold. Failures alert (`dataroom-quality-gate`), never silently pass.
- **Cross-source check (once book_top exists):** daily trade-price vs mid divergence stats — catches a silently wrong decimal/symbol mapping faster than any schema test.

## 12. Phasing

| Phase | Scope | Acceptance |
|---|---|---|
| **P0 — skeleton** | Collector (Coinbase BTC-USD `matches` + `ticker`) → bronze; normalizer → silver `trades` + `book_top`; instruments fetcher; catalog.sql; runbook | 7 consecutive days captured; gaps ledger accounts for 100 % of wall-clock; determinism test green; a DuckDB query over the week runs from a laptop |
| **P1 — RV product + backfill** | `vision-sync` + `binance_vision` adapter → full BTCUSDC tick history in silver; `gold/bars`, `gold/rv`, `gold/gaps`; signature-plot notebook | 2019→present BTCUSDC history queryable; BTC RV series across full grid (live Coinbase week **and** multi-year Binance history); documented recommended interval; cross-venue RV comparison (binance vs coinbase); comparison note vs mm-bot's live 300 s estimate |
| **P2 — breadth** | SUI spot adapter (Coinbase `SUI-USD`, plus SUI vision dumps via existing `vision-sync`); Hyperliquid funding adapter; `funding_rates` live; optional Coinbase REST trade backfill | New venues added with zero schema changes (the generalization test); cross-venue basis queryable |
| **P3 — options** | Deribit BTC chain ticker → `options_quotes`; instrument fetcher handles contract listing/expiry churn | Vol-surface history queryable: `options_quotes ⋈ instruments` reproduces a dated smile |
| **P4 — depth (deferred)** | L2 book deltas + snapshots (`book_deltas` table), book-replay correctness harness | explicitly out of v1 |

Each phase is independently shippable; P0+P1 alone deliver the stated first goal.

## 13. Design decisions & alternatives considered

- **Parquet+S3+DuckDB over TimescaleDB/ClickHouse:** backtests are columnar scans; the archive-of-record should be immutable files with no server. ClickHouse remains a compatible later addition *on top* (it reads Parquet); Timescale stays a serving layer for live products only. Nothing here re-ingests if we add either.
- **f64 prices over fixed-point int64 (Databento-style):** research store, tooling ergonomics win; bronze preserves exact strings as the escape hatch. Revisit only if a consumer needs exact accounting.
- **Collector in Rust over adopting cryptofeed:** the collector is ~small, the shop is Rust, and bronze-verbatim capture is the part worth owning; cryptofeed normalizes too early (at capture time) for a bronze-first design.
- **Hourly spool rotation over 5-min:** simplicity; durability budget in §6.4 is explicit and accepted.
- **Vision dumps archived as verbatim zips, only `kind=trades` normalized:** bronze-is-sacred extends to bulk sources; aggTrades would double-count the same fills if merged into silver `trades` (§6.6).
- **No orchestrator:** cron + idempotent partitions + catch-up scan covers v1's DAG (3 jobs). Revisit at ~10+ interdependent gold jobs.

## 14. Resolved questions (2026-08-12)

1. **Bucket/account placement** — resolved: defined in the existing `rust-backend/infra` Terraform root (same AWS account as the rest of the stack). Local-state gotchas apply (§3).
2. **Binance geo-restrictions** — resolved: assume blocked from our infra. Coinbase is the P0 venue; Binance re-enters at P2 only if reachability is solved (§12).
3. **P0 instrument** — resolved: BTC vs USDC → recorded as `btc-usd.coinbase`, since Coinbase's deep book is BTC-USD with USDC 1:1 on-platform (§6.5).
4. **Downstream serving from gold** — resolved: seeding the mm-bot's cold-start vol buffer is a confirmed *eventual* consumer, explicitly non-critical now. Design consequence: keep the daily normalizer cadence for v1, but don't build anything that assumes daily is forever (the partition-overwrite model already supports hourly runs unchanged).
