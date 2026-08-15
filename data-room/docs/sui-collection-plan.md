# SUI data collection plan

Status: **PLANNED, not started. Revised 2026-08-15** after verifying every
claim in it against the code and re-probing every endpoint. Written to be
picked up and executed by someone who has not been part of the analysis
behind it.

**Why this exists:** SUI is the launch underlying for the mm-bot V2 desk.
The findings driving every choice below are in
`docs/mm-bot-v2/07-backtest-data-and-cost-findings.md` — read §12 (open
questions) and §13 (what to build) before starting. This document is the
data-room half of that build list.

Spec: `docs/data-room-spec.md`. Ops: `data-room/docs/runbook.md`.

---

## 0. The organising principle

**Historic Binance data is a static archive. Live on-chain venue data is
not.**

Binance Vision has SUIUSDT back to 2023-05 and will still have it next
year; pulling it is a config change runnable any afternoon. Bluefin,
DeepBook, Cetus and Aftermath publish **no history at all** — their data
exists only if we are recording while it happens. Every day without a
collector is a day permanently missing.

So the priority order is inverted from intuition: **live on-chain venues
first, historic backfill last.** Tier 0 is urgent because it is
irrecoverable, not because it is more useful.

## 0.1 The finding that sets the urgency

Doc 07 §9.2 measures E[exercised notional] at ~1× NAV per turn, all of it
sold into Sui spot at expiry. Execution cost on that flow is the binding
capacity constraint on the strategy — and we have essentially no data on
it.

Measured 2026-08-15, selling SUI for USDC, DeepBook book-walk vs the
Aftermath router:

| size | DeepBook only | Aftermath router | router route |
|---|---|---|---|
| $10k | **13 bp** | 20 bp | Cetus, Obric |
| $50k | 41 bp | **31 bp** | Bluefin, Cetus |
| $100k | 50 bp | **35 bp** | Bluefin, Cetus |
| $250k | 80 bp | **39 bp** | Bluefin, Cetus |
| $500k | **exhausted** | **47 bp** | Bluefin, Cetus |
| $1M | **exhausted** | 238 bp | Bluefin, Cetus |

DeepBook `SUI_USDC` alone: spread 1.5 bp (tight), but only $3.9k of bid
depth within 10 bp, $40k within 50 bp, and $475k visible in total.

**The vault will integrate a swap router, so DeepBook is the floor, not
the plan.** Three things follow, and they shape what we collect:

1. **Collect both.** DeepBook is the guaranteed-available fallback *and*
   the flash-loan source; the router is the realistic execution. Neither
   substitutes for the other.
2. **The router is not always better** — below ~$25k DeepBook direct
   wins, because routing adds hops and fees. The crossover is a live
   decision the vault has to make per trade, so we need both series to
   calibrate it.
3. **Sui spot liquidity is not in DeepBook.** Every router path goes
   through Bluefin and Cetus. Anyone reasoning about our execution from
   DeepBook alone will be wrong in both directions — too pessimistic on
   cost, too optimistic on where the liquidity lives.

Capacity extends ~5–10× with routing, to somewhere past $1M rather than
~$500k. But at $1M the router costs 238 bp, which against ~1× NAV of
exercised notional exceeds the entire 1.72% round-trip edge (doc 07
§9.2). **There is still a ceiling; routing moves it, it does not remove
it.** We currently know its location from one snapshot.

That is the argument for starting Tier 0 now: not that the number is
alarming, but that it is a single sample of the quantity that decides how
large this strategy can get.

**How single a sample, measured 2026-08-15.** The $1M rung was quoted
three times over roughly one hour while building the collector for it:

| time (UTC) | $1M rung fill | vs spot |
|---|---|---|
| ~18:55 | 0.67956 | **41 bp** |
| ~19:24 | 0.66384 | **243 bp** |
| ~19:40 | 0.58912 | **1,346 bp** |

Same endpoint, same size, same direction; the last figure is three
identical consecutive probes, so it is a real state of the book and not
jitter. Doc 07's 238 bp sits in the middle of that range rather than
being the number.

This is the finding, and it is stronger than the one this section
originally made. The top of the ladder does not have a capacity *value*
— it has a **capacity distribution**, and it spans a factor of thirty
within an hour. A vault sized on any single snapshot of it is sized on
noise. Nothing in Tier 2 or Tier 1 can recover this; it exists only if we
are polling. Start the ladder.

---

## 0.2 How configuration actually reaches the host

**Read this before believing any "config only" estimate below.**

`deploy-data-room.yml` rewrites `TAG` and `ECR` in `/opt/data-room/.env`,
pulls both images, and runs `docker compose up -d collector`. That is all
it does. Its own comment is explicit: *"compose.yml + collector.toml + the
systemd units are provisioned on the host out-of-band."* Cloud-init
(`rust-backend/infra/data-room.tf`) does not write them either — it
installs docker, the AWS CLI, the SSM agent and the disk-metric cron, and
stops.

So every config change in this document is a **hand edit on the host over
SSM**, not a commit:

```bash
aws ssm start-session --target i-08033a8b4316826e7   # options-data-room-host
vi /opt/data-room/collector.toml
cd /opt/data-room && docker compose up -d collector   # reload
```

Mirror the same edit into `data-room/deploy/collector.toml.example` in git
so the state is not lost, but understand that committing it deploys
nothing. The same applies to `deploy/systemd/*` — editing those files and
running the workflow does not update the units on the host.

"Effort: minutes" below means minutes *given this*. Someone who edits the
example file, commits, and runs Deploy will see no change and lose an
afternoon.

---

## 1. Tier 0 — start now, irrecoverable

### 1.1 DeepBook SUI/USDC depth — **config only**

The Mysten public indexer serves L2 depth over plain HTTP, so this needs
no Sui RPC, no `sui_tx` dependency, and no violation of the spec's
"zero runtime dependencies on the protocol stack" rule.

Verified working 2026-08-15:

```
GET https://deepbook-indexer.mainnet.mystenlabs.com/orderbook/SUI_USDC?level=2&depth=100
  -> {"timestamp":"1786819322214","bids":[["0.68014","1127.8"],...],"asks":[...]}
```

Gotchas found while probing:
- **Both `level` and `depth` are required.** The bare URL returns
  `RPC error: No results from simulate_transaction`.
- `depth=100` returns 50 levels per side. `depth=200` errors — 100 is the
  practical maximum. Do not assume larger works.
- Pool metadata: `GET /get_pools`. `SUI_USDC` pool_id is
  `0xe05dafb5133bcffb8d59f4e12465dc0e9faeaa05e3e342a08fe135800e3e4407`,
  base decimals 9, quote decimals 6, lot_size 100000000, tick_size 100.

Add to `collector.toml` — the existing `Poller` struct
(`collector/src/main.rs`, fields `exchange` / `url` / `stream` /
`interval_secs`) already does exactly this, and pollers write bronze
under their fixed `stream` name with **no `route_payload` arm needed**:

```toml
[[pollers]]
exchange      = "deepbook"
url           = "https://deepbook-indexer.mainnet.mystenlabs.com/orderbook/SUI_USDC?level=2&depth=100"
stream        = "book.SUI_USDC"
interval_secs = 30
```

`/ticker` takes **no pool argument** — one poll returns all 26 pools
including `SUI_USDC` volume — so a single 60 s poller covers venue-wide
volume in one line, rather than one per pool. `DEEP_SUI`
(`0xb663828d6217467c8a1838a03793da896cbe745b150ebd57d82f814ca579fc22`) is
worth a third orderbook poller: it is the DEEP-fee source.

**Effort:** minutes, per §0.2. **Gate:** bronze objects landing under
`exchange=deepbook/stream=book.SUI_USDC/`; one day of files parses as JSON
with monotonic timestamps.

### 1.1b Aggregator quote ladder — **needs a small Poller extension**

Per §0.1 the router, not DeepBook, is the realistic execution path above
~$25k. Capture it as a **quote ladder**: poll the expected output for a
fixed set of sizes, which measures execution cost directly rather than
requiring us to reconstruct it from books across five protocols.

Verified working 2026-08-15 (note: **POST**, not GET):

```
POST https://aftermath.finance/api/router/trade-route
Content-Type: application/json
{"coinInType":"0x2::sui::SUI",
 "coinOutType":"0xdba…::usdc::USDC",
 "coinInAmount":"<amount, 9 decimals>"}
  -> {"routes":[{"paths":[{"protocolName":"Cetus",…}]}],"coinOut":{"amount":"…n"}}
```

Response quirk: numeric amounts are strings with a trailing `n`
(JS BigInt literal) — `.rstrip("n")` before parsing.

**This does not fit the existing `Poller`,** which does
`http.get(&poller.url)` (`collector/src/main.rs` ~line 182). Extend it
with optional `method` and `body` fields, defaulting to GET so every
existing poller is unchanged. ~10 lines.

Then one poller per rung — sizes chosen to straddle the crossover and the
ceiling:

```toml
[[pollers]]
exchange      = "aftermath"
url           = "https://aftermath.finance/api/router/trade-route"
method        = "POST"
body          = '{"coinInType":"0x2::sui::SUI","coinOutType":"0xdba…::usdc::USDC","coinInAmount":"14700000000000"}'
stream        = "quote.SUI-USDC.10k"
interval_secs = 300
```

Ladder: ~$10k / $50k / $250k / $1M of SUI. Both directions if the vault
will ever buy underlying, not just sell it. 5-minute cadence is ample —
this is a depth-regime series, not a tick feed.

**Effort:** ~1 hour including the Poller extension. **Gate:** bronze
landing per rung; output amounts parse and track spot sensibly.

### 1.2 Bluefin SUI-PERP — **needs code**

The hedge venue (see `docs/mm-bot-v2/06-dbm-removal.md` and 07 §6.2).
This answers doc 07 open question #2, which swings the value of passive
hedging by 2.5×.

Bluefin Pro publishes **public, unauthenticated** market-data websockets —
no API key, unlimited connections. Message types include
`OrderbookDiffDepthUpdate` and `OrderbookPartialDepthUpdate` with stream
variants at 10 ms / 200 ms / 500 ms, plus trades, tickers and candles.

**Take the 200 ms diff depth.** 10 ms is ~20× the bronze volume for no
analytical gain at our decision horizon.

> **Confirm before coding:** I did not verify the websocket URL or exact
> subscribe frame shape — only that the public streams exist and their
> message-type names. The repo already pins the Bluefin Pro REST hosts in
> `rust-backend/services/hedge-signer/config/config.staging.toml`
> (`auth.api.sui-staging` / `api.sui-staging` / `trade.api.sui-staging`,
> prod at `api.sui-prod.bluefin.io`) — start there, then confirm the
> stream host and frame shape against the `@bluefin-exchange/pro-sdk`
> package (`src/docs`, `ExchangeDataApi`) or
> <https://bluefin-exchange.readme.io/>. Do not copy a URL out of this
> document.

**Five** seams, not three. The first two are the ones that bite:

| Location | Change | Why it matters |
|---|---|---|
| exchange allowlist, `main.rs:233` | add `"bluefin"` to `matches!(…, "coinbase" \| "hyperliquid")` | it `bail!`s — a bluefin block **aborts the whole daemon at startup**, taking BTC capture down with it |
| `marker_streams()`, `main.rs:281` | add bluefin arms | without markers there are no connect/disconnect rows, so **the reconnect gate below cannot pass** |
| `subscribe_msgs(conn: &Connection)` | add a `"bluefin"` arm emitting the subscribe frame(s) | |
| `route_payload(exchange, payload)` | add `"bluefin" => adapters::bluefin::route(payload)` | |
| `keepalive(exchange)` | add a ping if Bluefin drops quiet connections (Hyperliquid does; check) | |

Plus `crates/adapters/src/bluefin.rs` with a `route(payload) -> Option<String>`
mapping each frame to a bronze stream name (`book.SUI-PERP`,
`trades.SUI-PERP`, `ctx.SUI-PERP`). Follow `adapters/src/hyperliquid.rs`
as the closest model.

One adjacent one-liner, cheap to do here rather than in S5:
`gold/src/gaps.rs:55` has `const EXCHANGES: &[&str] = &["coinbase",
"hyperliquid"]`. Without `"bluefin"` the gaps ledger silently never audits
it, and the reconnect drill's gaps row never appears.

Then:

```toml
[[connections]]
exchange = "bluefin"
url      = "<from SDK docs>"
products = ["SUI-PERP"]
channels = ["orderbookDepthDiff200ms", "trades", "ticker"]   # confirm names
```

**Effort:** ~half a day including a golden fixture. **Gate:** bronze
landing for all three streams; a captured frame committed as a fixture in
`crates/adapters/fixtures/`; reconnect drill per the spec's R1 gate
(kill the socket, confirm marker rows **and** a gaps row).

### 1.3 Aftermath SUI-PERP — **optional, defer**

Distinct from 1.1b: that is the *router*, this is the *perp venue*. Only
valuable if we might revisit the venue decision (doc 07 §6.2 records
Bluefin as the call, with §9.3 as the one argument the other way).
Cheap if the Bluefin adapter generalises; skip entirely if it does not.
**Do not let this delay 1.1 or 1.2.**

---

## 2. Tier 1 — config only, same week

Both venues are confirmed live for SUI and both existing adapters are
symbol-agnostic, so these are one-line changes with no code:

```toml
# collector.toml — extend the existing blocks, do not add new ones
[[connections]]
exchange = "coinbase"
products = ["BTC-USD", "SUI-USD"]        # SUI-USD confirmed status=online
channels = ["matches", "ticker", "heartbeat"]

[[connections]]
exchange = "hyperliquid"
products = ["BTC", "SUI"]                # confirmed listed, maxLeverage 10
channels = ["trades", "bbo", "activeAssetCtx"]
```

Neither is a venue we trade, but they give a liquid cross-venue reference
and a sanity check on the on-chain books. `activeAssetCtx` yields mark,
funding and OI for free.

**Gate:** new bronze stream partitions appear within the hour; the
existing `normalizer coinbase` / `normalizer hyperliquid` runs pick up
the new symbols with no code change (verify — this is also the spec's
generalisation test).

---

## 3. Tier 2 — historic backfill, no urgency

### 3.1 Config-only

`vision-sync` takes `--symbols` / `--kinds` / `--market` as CLI args, and
`binance_vision::split_symbol` already resolves `SUIUSDT → (SUI, USDT)`.
**No code change.**

```bash
docker compose run --rm batch vision-sync --symbols SUIUSDT,SUIUSDC --kinds trades
docker compose run --rm batch vision-sync --market um --symbols SUIUSDT --kinds trades,fundingRate
docker compose run --rm batch normalizer vision --symbols SUIUSDT,SUIUSDC
docker compose run --rm batch normalizer vision --market um --symbols SUIUSDT
```

Coverage verified against the Vision bucket 2026-08-15:

| Dump | Coverage | Files |
|---|---|---|
| SUIUSDT spot trades | 2023-05 → 2026-07 | 39 monthly |
| SUIUSDT perp trades | 2023-05 → 2026-07 | 39 monthly |
| SUIUSDT fundingRate | 2023-05 → 2026-07 | 39 monthly |
| SUIUSDC spot trades | 2024-01 → 2026-07 | 31 monthly |

Prefer **SUIUSDC** where both exist — it is the USDC-quoted book, closer
to our settlement asset, at the cost of 8 fewer months.

Standing Vision gotchas (all have regression tests, see the memory and
`normalizer/src/vision.rs`): duplicate CSVs under `fsx-data/…` in some
zips, some months not time-ordered, ms vs µs epochs, futures CSVs carry a
header and lowercase bools, monthly/daily overlap.

**Those four commands are one-shots.** The recurring units are BTC-only
and must be edited on the host (§0.2) or the SUI dailies simply stop
arriving once the backfill finishes:

- `data-room-vision-sync.service` — hardcodes the `BTCUSDC` default and
  `--symbols BTCUSDT`; add the SUI symbols to both ExecStart lines.
- `data-room-normalizer.service` — has **no `normalizer vision` line at
  all**. Vision normalization is manual today, for BTC as well as SUI.
  Adding one closes a pre-existing gap.
- `data-room-instruments.service` — BTC-only defaults; add SUI to
  `--coinbase-products`, `--binance-symbols`, `--binance-perp-symbols`
  and `--hyperliquid-coins`. The instrument builders are symbol-generic,
  so this is args only.

**Gate:** row counts per day reconcile against dump line counts;
spot-check a month against Binance's published klines; the next timer
firing picks up a new daily without hand-holding.

### 3.2 Needs code

**`bookTicker` — the only historical spread data that exists for SUI.**
2023-05-16 → 2024-03-30, 12 monthly / 320 daily files, then discontinued.
This is what lets us **measure** the passive fill fraction in doc 07 §7
rather than assume it. Highest-value item in Tier 2.

Scope is more than one parse function:

- `parse_book_ticker_csv` in `crates/adapters/src/binance_vision.rs`
  alongside the existing `parse_trades_csv` / `parse_funding_csv`.
- `normalize_book_ticker_zip` in `normalizer/src/vision.rs`, writing
  `silver_key("book_top", "binance", "SUI-USDT-PERP", day)`. The
  `book_top` schema already exists and its `ts_event` is nullable
  precisely for this case.
- A third arm in `normalize_pending` (`vision.rs:50`), whose `kinds` list
  is currently hardcoded to `["trades", "fundingRate"]` for um-futures.
- `--kinds bookTicker` on the vision-sync invocation.

**Capacity is the real constraint here, more than for anything in Tier 0.**
Measured sizes: 12 monthly zips at **0.33–1.76 GB each, ~9.7 GB
compressed total**; 2024-01 alone is 1.76 GB, and uncompressed CSV is
several times that against a 100 GB root volume. `normalize_zip` buffers
one UTC day of rows in memory before serialising, and a day of
tick-cadence BBO is far more rows than a day of trades. Dry-run against
the smallest zip (2023-05, 327 MB) before touching 2024-01, and use
`--since` to run bounded batches so the volume never holds more than a
month unzipped. (The header comment in `normalizer/src/vision.rs` says
"the host has 2 GB" — it is a t3.medium with 4 GB. Stale, and
load-bearing for this sizing.)

**`premiumIndexKlines` 1h** — mark price / premium index, i.e. perp-spot
basis history, which feeds the funding model. Nice to have, but **not "a
small adapter"**: Vision nests an interval directory
(`…/premiumIndexKlines/SUIUSDT/{1m,5m,…,1mo}/`, 16 of them) and
`vision-sync` builds `data/{market}/{period}/{kind}/{symbol}/` with no
slot for it. Passing `--kinds premiumIndexKlines/1h` produces both the
wrong path order and a slash inside a hive partition value in
`bronze_vision_prefix`. Without a vision-sync path change you would
mirror ~474 zips across all 16 intervals. There is also no silver
klines/premium table to land it in. Its own ticket, not a rider on
bookTicker.

### 3.3 Explicitly skip

- **`aggTrades`** — same fills as `trades`; the spec forbids merging them
  into silver `trades` (double counting). Archive-only at best.
- **`metrics` (open interest)** — **does not exist for SUIUSDT**. Zero
  files on Vision. Record the gap; do not go looking for it.

---

## 4. What we will not get, at any price

- **No SUI open-interest history** (§3.3).
- **No Bluefin / Aftermath / Cetus / DeepBook history.** Series start the
  day collectors go on. This is the entire argument for Tier 0.
- **No SUI options data anywhere.** We are first to market; that is the
  product thesis and also why doc 07 §3's IV ablation must run on BTC.
- **No flash-loan borrow ceiling from any of this** (§7.3). No poller in
  this document produces `quote_balance`; it needs an on-chain read.

## 4.1 What is not wired: telemetry

The collector exports `dataroom_collector_messages_total` and
`dataroom_collector_last_message_unix_seconds` on `:9100`, but **nothing
scrapes the data-room host** — there is no data-room job in
`rust-backend/deployment/monitoring/prometheus.yml`. The
`dataroom-collector-stalled` alert referenced in the spec's R1 gate and
in the runbook exists only as a string; there is no rule anywhere in the
repo.

Consequence for every gate in this document: **check bronze in S3
directly** (`aws s3 ls s3://options-data-room-20260813122351104900000001/bronze/v1/…`).
Do not plan on a metric firing. Wiring the scrape plus the stall rule is
worth its own step, and it should precede S2 — a Bluefin socket that dies
quietly is exactly the failure this is missing.

---

## 5. The shortcut: bronze first, silver later

Bluefin L2 depth wants a `book_deltas` table, which is **P4 in the spec
and does not exist**. Do not let that block capture.

The collector does not parse on the hot path — it stamps `ts_recv_ns`,
increments `seq`, and appends the raw frame to the spool. The only
per-venue code is the `route_payload` arm choosing a bronze stream name.
So:

**Land bronze capture for Bluefin and DeepBook now; defer the normalizer
and the P4 schema entirely.** Bronze-is-sacred means nothing is lost — the
silver layer can be built next month and replay every frame recorded in
the meantime. The expensive, irreversible thing is not recording.

---

## 6. Sequencing

| Step | Work | Gate before proceeding |
|---|---|---|
| **S0** | Prometheus scrape of the data-room host + `dataroom-collector-stalled` rule (§4.1) | alert fires and resolves on a deliberate collector stop |
| **S1** | DeepBook poller (1.1) + router quote ladder (1.1b) + Coinbase/Hyperliquid SUI config (§2) | bronze landing for all streams; existing pollers unchanged by the GET/POST extension |
| **S2** | Bluefin bronze capture (1.2), incl. the allowlist, markers and `gaps.rs` | bronze landing; fixture committed; reconnect drill passes with marker **and** gaps rows |
| **S3** | Vision SUI backfill (3.1) **and the three systemd units** | row counts reconcile; 2023-05 → now queryable via `catalog.sql`; next timer firing picks up a daily unattended |
| **S4** | `parse_book_ticker_csv` + normalizer arm (3.2) | dry-run on 2023-05 fits in RAM/disk; golden test green; `book_top` populated for 2023-05 → 2024-04 |
| **S5** | P4 `book_deltas` schema + Bluefin/DeepBook normalizers | replay S2's accumulated bronze; determinism test green |

S1 and S2 are the urgent ones. S3–S5 can happen whenever. S0 is cheap and
makes all of the rest observable.

**Host-capacity note, before S2 and again before S4.** The host is a
t3.medium / 100 GB (92 GB free as of 2026-08-15) sized for a few MB/day
plus one Deribit chain poll.

- *S2:* Bluefin 200 ms diff depth on one market is materially more bronze
  than anything currently collected — estimate the daily volume against
  the 100 GB volume and the S3 lifecycle rules (`bronze` → IA at 30d,
  Glacier IR at 180d) before turning it on.
- *S4:* ~9.7 GB compressed of bookTicker, several times that unzipped,
  plus a one-day in-memory row buffer. This is the tighter of the two,
  and the plan's original draft flagged capacity only for S2.

The disk alarm at 80% exists but has already been learned the hard way
once (see the `options-host disk full` history).

---

## 7. Open questions for whoever picks this up

1. **Bluefin websocket URL and subscribe frame shape** — deliberately not
   asserted here (§1.2). Get from the SDK, not from this doc.
2. **Which router will the vault actually integrate?** §1.1b measures
   Aftermath's because it has a verified public quote API. If the vault
   ships with a different one (7k, Cetus aggregator, Hop), point the
   ladder at that instead — the whole value is that the series matches
   the execution path we will really use.
3. **What is the flash-loan borrow ceiling?** A swap router lifts the
   *swap* constraint but not the *borrow* one — see doc 07 §9.3(2).
   `borrow_flashloan_quote` needs one pool holding the full strike
   notional, and `quote_balance` is not exposed by the indexer's
   orderbook endpoint. It needs an on-chain read of the pool object,
   which is the one item here that may justify a Sui RPC dependency.
   Currently the least-understood constraint in the design.
4. **Does Cetus expose a pollable quote or pool-state endpoint?** It is on
   every router path above $50k, so it is part of the depth that binds,
   and it is currently uncollected. Not investigated.
5. **Retention** — is 200 ms depth worth keeping at full fidelity
   forever, or should the normalizer downsample into silver and let
   bronze age into Glacier? Spec says nothing auto-expires; revisit.

**Resolved, recorded so they are not re-asked:**

- *Which DeepBook pool — `SUI_USDC` or a DBUSDC variant?* `SUI_USDC`,
  `0xe05dafb5…`. There is no `SUI_DBUSDC` on mainnet; DBUSDC is our
  testnet token.

---

## 8. Verified reference (2026-08-15)

Facts established by direct probing while writing this, so the next
person does not have to re-derive them:

- Coinbase `SUI-USD`: `status=online`, `quote_increment=0.0001`.
- Hyperliquid `SUI`: listed, `maxLeverage=10`, `szDecimals=1`.
- DeepBook indexer: `/get_pools`, `/ticker`, and
  `/orderbook/{pool}?level=2&depth=N` all public and unauthenticated;
  `depth=100` → 50 levels/side, `depth=200` errors.
- Aftermath router: `POST /api/router/trade-route` on
  `aftermath.finance`, public and unauthenticated, returns routes plus
  `coinOut.amount`; numeric fields are BigInt-style strings ending in
  `n`. Routes SUI→USDC through Bluefin/Cetus/Obric, not DeepBook.
- The $1M rung moved 41 bp → 243 bp → 1,346 bp inside one hour on
  2026-08-15 (§0.1). Treat every single-snapshot depth number in this
  document, including the ones above, as one draw from a wide
  distribution.
- Execution ladder (both venues) per §0.1.
- Binance Vision SUI coverage per §3.1; `metrics` empty; `bookTicker`
  ends 2024-03-30.
- DeepBook `SUI_USDC` depth profile per §0.1.
- Bluefin fees (mainnet): maker 0.010%, taker 0.035% + flat 0.03 USDC
  gas on taker trades; place/cancel/reprice free. Aftermath: maker
  −0.005% (promotional), taker 0.045%. Sourced from doc 07 §6, not
  re-probed here.
- `premiumIndexKlines` nests 16 interval subdirectories under the symbol;
  `bookTicker` monthly zips are 0.33–1.76 GB each.

**Code, verified at the line cited**

- `Poller` is GET-only: `collector/src/main.rs:182`.
- Exchange allowlist that `bail!`s: `collector/src/main.rs:233`.
- `marker_streams` arms: `collector/src/main.rs:281`.
- Gaps ledger exchange list: `gold/src/gaps.rs:55`.
- `normalize_pending` hardcoded kinds: `normalizer/src/vision.rs:50`.
- `bronze_vision_prefix` (slash-unsafe): `crates/schema/src/lib.rs:501`.
- `book_top` schema, nullable `ts_event`: `crates/schema/src/lib.rs:137`.
- Symbol-agnostic ws normalization: `normalizer/src/ws.rs`
  (`streams_for_date` lists bronze; `stream_target` matches on stream
  kind, never on the symbol suffix).
- Spec's "zero runtime dependencies on the protocol stack": spec §276.
- P4 `book_deltas` deferred: spec §343. R1 marker-row gate: spec §316.

**Infra**

- Host `i-08033a8b4316826e7`, t3.medium, 100 GB gp3, 92 GB free.
- Bucket `options-data-room-20260813122351104900000001`.
- Lifecycle: bronze → STANDARD_IA at 30 d → GLACIER_IR at 180 d.
- Disk alarm at 80%; no SNS action (Grafana handles alerting).
- No Prometheus scrape of this host (§4.1).
