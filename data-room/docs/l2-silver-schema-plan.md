# L2 depth: silver schema + gold slippage surface

Status: **silver SHIPPED (SO-446, 2026-09-02): `book_l2` + `quote_ladder`
schemas, Bluefin/DeepBook/Aftermath adapters and normalizers, determinism
tests. `gold/depth`, `seq_gap`, quality gates and timers (§5, §6 gold
rows, §8) remain open.** Originally planned 2026-08-15; promotes spec
§13 P4 ("depth — deferred") into scope, because the backtesting
framework's execution model has no other input.

Companion docs: `docs/data-room-spec.md` (§4 layout, §5 schemas, §7
normalizer contract), `sui-collection-plan.md` (the collectors producing
this bronze), `docs/mm-bot-v2/08-backtesting-framework.md` §4.4 (the
consumer).

> Path note: if the monorepo move in 08 §3 happens first, these paths
> shift under `rust-backend/`. Nothing else changes.

---

## 1. Why `book_top` is not enough

`silver/book_top` carries one level per side. Every cost conclusion in
`docs/mm-bot-v2/07-backtest-data-and-cost-findings.md` is **fees-only,
zero slippage**, and 07 §14 lists that as the dominant omission. The
measurements that matter — hedge rebalance cost at 10–20% bands, and the
exercise-unwind cost that sets the strategy's capacity ceiling — are
slippage problems, not fee problems.

One live snapshot already showed the shape of the risk: DeepBook
`SUI_USDC` quotes a 1.5 bp spread but holds only $3.9k within 10 bp.
Top-of-book is actively misleading here — tight and thin look identical
until you measure depth.

---

## 2. Three shapes of source data, not one

The collectors produce structurally different things, and forcing them
into one table would be wrong:

| Source | Shape | Cadence |
|---|---|---|
| Bluefin WS `OrderbookDiffDepthUpdate` | sequenced **diffs** | 200 ms |
| Bluefin REST `/v1/exchange/depth` (was: WS `OrderbookPartialDepthUpdate` — top-20 only, see §3) | full **snapshots** | 60 s poll |
| DeepBook indexer `/orderbook?level=2&depth=100` | full **snapshots**, 50 levels/side | 30 s poll |
| Aftermath router `/trade-route` | **quote ladder** (size in → amount out) | 5 min poll |

Diffs and snapshots share a representation. The quote ladder does not —
it is already a derived execution curve, not a book.

⇒ **Two new silver tables, one new gold table.**

---

## 3. `silver/book_l2`

One row per price level per update. Snapshots and diffs share the table,
discriminated by `is_snapshot`.

```
silver/v1/book_l2/exchange=<x>/symbol=<s>/date=<d>/part-00.parquet
```

| column | type | notes |
|---|---|---|
| `ts_event` | i64 ns, nullable | venue timestamp |
| `ts_recv` | i64 ns | capture time. **Never null** — this table is live-capture only, no archive source exists |
| `exchange` | dict str | |
| `instrument_id` | dict str | ours, not venue-native |
| `seq` | i64 | venue sequence at the **end** of the update (Bluefin `lastUpdateId`; DeepBook: venue timestamp ms) — ordering **and** dedup |
| `seq_first` | i64, nullable | venue sequence at the **start** of a diff (Bluefin `firstUpdateId`); null on snapshots. Gap detection: `seq_first != prev.seq + 1` |
| `is_snapshot` | bool | true ⇒ this row belongs to a full-book image at `seq` |
| `side` | dict str | `bid` \| `ask` |
| `price` | f64 | |
| `size` | f64 | **absolute size at this level after the update; `0` = level removed** |
| `src_file` | dict str | lineage |
| `src_line` | i32 | lineage |

**`size` is absolute, never a delta.** Both Bluefin diff-depth and
DeepBook snapshots publish level-replace semantics, and absolute sizes
make reconstruction idempotent — a dropped message corrupts one level
until its next update, rather than permanently desynchronising an
accumulator.

Row sort for determinism (spec §7): `(ts_recv, seq, side, price)`,
lineage as the final tiebreak. Writer settings from the existing
`schema::writer_props()`.

**Shipped decisions (SO-446), recorded so they are not re-litigated:**

- `seq_first` was added. The plan's single `seq` cannot detect a Bluefin
  gap: frames span `firstUpdateId..lastUpdateId`, so `lastUpdateId`
  legitimately jumps every frame. Verified on the live socket
  (2026-09-01): consecutive frames satisfy `first == prev.last + 1`, so
  `seq_first != prev.seq + 1` is the gap test.
- **Bluefin snapshots come from the REST `GET /v1/exchange/depth`
  poller (`depth.{symbol}` bronze, 60 s), not from the WS
  `Partial_Depth_*` streams.** Partial depth is top-5/10/20 only —
  probed live, `depthLevel: "20"` — so it is not a book image and the
  normalizer parses those frames to nothing. The REST response is the
  venue's documented diff-stream sync point (its `lastUpdateId` falls
  inside one diff's `first..last`); `limit=1000` is the max and today
  returns the whole visible book (~50 levels/side). Its
  `updatedAtMillis` is `0`, so those rows have `ts_event = null`.
- Frame dedup key is `(is_snapshot, seq)` across the day: a reconnect
  re-capture or a DeepBook response served twice with the same
  `timestamp` lands once, first capture wins (bronze keys are sorted).
- Normalizer memory is bounded by **bronze hour**, not by day: an hour
  directory is a `ts_recv` boundary, so per-hour sort + one row group
  per hour (`schema::BookL2Writer`) preserves the global order without
  ever holding a day of rows. A separate `normalizer::book_l2` module
  rather than `ws.rs` arms, because `ws.rs` collects a whole day into a
  `Vec` (fine for trades, not for 1–2M diff rows on a 2 GB host).
- An empty diff (both sides empty) produces no rows and no dedup entry.
- Symbols: Bluefin `SUI-PERP` → `sui-perp.bluefin` / partition
  `SUI-PERP`; DeepBook pool `SUI_USDC` → `sui-usdc.deepbook` / partition
  `SUI-USDC`.

### 3.1 Reconstruction contract

```
book_at(T) = fold(
    latest snapshot S where S.seq <= T,
    all rows with S.seq < seq <= T
)
```

This imposes an **operational requirement on the collector**: every
partition must contain at least one full snapshot, ideally several. Two
consequences:

- **Bluefin capture must poll `GET /v1/exchange/depth` alongside the
  diff stream** (superseding the original "subscribe to
  `OrderbookPartialDepthUpdate`" wording — see the shipped decisions
  above for why partial depth cannot serve). Diffs alone make a day's
  partition unreconstructable without replaying the previous day, which
  breaks the partition-independence the whole design rests on.
- A partition whose first snapshot is at 04:00 UTC cannot be
  reconstructed before 04:00. That is honest and acceptable — record it,
  do not paper over it (see §5).

DeepBook needs none of this: every poll is a snapshot.

### 3.2 Sequence gaps

If `seq` jumps, the book is **stale** until the next snapshot. This is not
the same as a capture gap and needs its own treatment: extend the
`gold/gaps` ledger with `kind = "seq_gap"`, spanning `[gap_seq_ts,
next_snapshot_ts)`.

Backtests must refuse to read depth inside a stale window exactly as they
refuse to trade inside a capture gap (08 §4.1). Silent staleness here
produces confidently wrong slippage numbers.

---

## 4. `silver/quote_ladder`

The router poller's output. Not a book — a measured execution curve.

```
silver/v1/quote_ladder/exchange=<x>/pair=<p>/date=<d>/part-00.parquet
```

| column | type | notes |
|---|---|---|
| `ts_recv` | i64 ns | never null — live capture only |
| `exchange` | dict str | `aftermath`, later others |
| `pair` | dict str | `BASE-QUOTE`, e.g. `SUI-USDC`, **for both directions** |
| `direction` | dict str | `sell_base` (base in, quote out) \| `buy_base` (quote in, base out) |
| `amount_in` | f64 | human units of the coin sent in |
| `amount_out` | f64 | human units of the coin quoted out |
| `route` | str, nullable | protocols traversed in order of first appearance, e.g. `Cetus,Bluefin` — diagnostic |
| `src_file`, `src_line` | | lineage |

**Shipped (SO-446, S1c).** `schema::QuoteLadder` / `quote_ladder_batch`,
`adapters::aftermath::parse`, `normalizer aftermath`. Decisions taken
while shipping, recorded so they are not re-litigated:

- The column is `direction`, not `side` — `side` already means bid/ask
  in `book_l2`, and the two tables are joined in `gold/depth`.
- Direction is derived from the **payload**, not the stream name: the
  coin type on `coinIn` / `coinOut` decides. USDC is the quote coin; the
  other leg is the base. Every `route.SUI-USDC.*` row captured before
  the buy-base pollers existed therefore normalizes as `sell_base` with
  no migration — there were no silver rows to default.
- Coin decimals come from a fixed coin-type table (`SUI` 9, `USDC` 6)
  matched on the `::module::Name` suffix. An unknown coin is a reject,
  never a guess.
- Row order: `(ts_recv, direction, src_file, src_line, amount_in)`.
- Buy-base rungs are **fixed USDC** (`route.USDC-SUI.<usdc>`), so those
  stream names are dollar sizes; sell-base rungs stay fixed SUI. The
  stream name is never read by the normalizer.

Slippage is derived, not stored: it needs a reference mid, which is a
gold-layer join. Keep silver a faithful record of what the venue quoted.

Note the response quirk recorded in `sui-collection-plan.md` §1.1b —
Aftermath returns BigInt-style strings with a trailing `n`. The adapter
strips it; the golden fixtures (`aftermath-trade-route-{sui-usdc,
usdc-sui}.json`, real responses) cover both directions.

---

## 5. `gold/depth` — the table the backtest actually consumes

**Backtests must not replay L2.** Reconstructing a book for every step of
every sweep cell is prohibitively expensive and would be recomputed
identically thousands of times. Precompute the execution curve instead —
the same philosophy as `gold/rv`.

```
gold/v1/depth/exchange=<x>/symbol=<s>/date=<d>/part-00.parquet
```

| column | type | notes |
|---|---|---|
| `ts` | i64 ns | slot end |
| `exchange`, `instrument_id` | dict str | |
| `side` | dict str | `sell_base` \| `buy_base` |
| `size_usd` | f64 | ladder rung |
| `avg_price` | f64 | volume-weighted fill price |
| `slippage_bps` | f64 | vs mid at `ts` |
| `levels_consumed` | i32 | book-derived only |
| `exhausted` | bool | true ⇒ the rung exceeds visible depth |
| `source` | dict str | `book` \| `router` |
| `coverage` | f64 | fraction of the slot with a usable book (staleness/gaps) |

Rungs: `{1k, 5k, 10k, 50k, 100k, 500k, 1M}` USD. Slot: 60 s.
⇒ 7 × 2 × 1440 ≈ **20k rows/day** — trivial, and it turns the execution
model into a lookup with interpolation.

**This unifies both silver tables.** DeepBook and Bluefin rows arrive via
book-walk (`source=book`); Aftermath rows come straight from
`quote_ladder` (`source=router`) with no reconstruction at all. The
execution model then compares them per trade, which is exactly the
decision doc 07 §9.2 showed the vault must make (DeepBook wins below
~$25k, the router above).

`coverage` mirrors `gold/rv`'s convention so consumers filter the same way.

---

## 6. Code changes

Precise seams, all of which already exist:

| File | Change |
|---|---|
| `crates/schema/src/lib.rs` | ✅ `BookL2` / `QuoteLadder` structs (typed, like `OptionsQuote` — no new `CanonicalEvent` variants were needed); `book_l2_schema()` / `book_l2_batch()`, `quote_ladder_schema()` / `quote_ladder_batch()` / `quote_ladder_key()`; `BookL2Writer` mirroring `TradesWriter` (one flushed row group per chunk) |
| `crates/adapters/src/bluefin.rs` | ✅ `parse_book()` (WS diffs → `BookL2`), `parse_depth_rest()` (REST snapshot → `BookL2`, `is_snapshot=true`); real fixtures `bluefin-diffdepth{,-multi}.json`, `bluefin-partialdepth.json`, `bluefin-depth-rest.json` |
| `crates/adapters/src/deepbook.rs` | ✅ `parse_book()` → `BookL2` with `is_snapshot=true`; real fixture `deepbook-orderbook.json` |
| `crates/adapters/src/aftermath.rs` | ✅ `parse()` → `QuoteLadder` (strip trailing `n`); real fixtures both directions |
| `normalizer/src/book_l2.rs` | ✅ Bluefin (`book.*` + `depth.*`) and DeepBook (`book.*`) → `book_l2`, hour-chunked; determinism tests. (`ws.rs` deliberately untouched — see §3 decisions) |
| `normalizer/src/aftermath.rs` | ✅ `route.*` → `quote_ladder` |
| `normalizer/src/main.rs` | ✅ `bluefin`, `deepbook`, `aftermath` subcommands |
| `gold/src/depth.rs` | new: book replay + ladder walk → `gold/depth` |
| `gold/src/main.rs` | `depth` subcommand |
| `gold/src/gaps.rs` | emit `kind="seq_gap"` |
| `catalog/catalog.sql` | ✅ views `book_l2`, `quote_ladder`; `depth` pending |
| `deploy/systemd/data-room-normalizer.service` | ✅ `aftermath`, `bluefin`, `deepbook` lines (hand-installed on the host, per `sui-collection-plan.md` §0.2) |
| `deploy/collector.toml.example` | ✅ `depth.SUI-PERP` REST snapshot poller (60 s) — must be mirrored onto the live host before Bluefin partitions are reconstructable |
| `deploy/systemd/data-room-gold.service` | add `gold --date $d depth` after `bars` |

`deribit.rs` is the closest existing model for the poller-snapshot path;
`hyperliquid.rs` for the WS-diff path.

---

## 7. Volume and cost

Rough sizing, to be checked against the host before enabling:

| Stream | Est. rows/day | Est. bronze/day |
|---|---|---|
| Bluefin 200 ms diff depth, 1 market | 1–2 M | ~50–100 MB |
| Bluefin periodic snapshots | small | small |
| DeepBook 30 s × 100 levels | ~288 k | ~10 MB |
| Aftermath ladder, 4 rungs × 5 min | ~1 k | negligible |

That is one to two orders of magnitude more bronze than the data room
currently produces (a few MB/day plus one Deribit chain poll). Two
consequences:

1. **Check the 100 GB root volume before enabling Bluefin.** The host has
   been wedged by disk pressure before; the 80% CloudWatch alarm exists
   but is the last line, not the plan.
2. Existing S3 lifecycle rules already handle the tail (bronze → IA at
   30 d, Glacier IR at 180 d). Silver `book_l2` stays standard and is the
   thing to watch — consider whether it needs a lifecycle rule of its own,
   since unlike every other silver table it is genuinely large.

If volume is a problem, drop to 500 ms diff depth before dropping levels —
the execution model cares about depth shape far more than update latency.

---

## 8. Quality gates

Added to the existing daily gate (spec §11, `dataroom-quality-gate`):

- **Crossed book**: reconstructed best bid ≥ best ask ⇒ fail. The single
  best detector of a broken diff/snapshot merge.
- **Negative or NaN sizes** ⇒ fail.
- **`seq` monotonic** within (exchange, instrument) ⇒ else emit `seq_gap`.
- **Snapshot cadence**: at least one snapshot per partition, warn below a
  configured hourly target.
- **Determinism**: normalize a fixture partition twice → byte-identical
  parquet (spec §7, existing CI pattern).
- **Cross-check**: `gold/depth` at the 1k rung should track `book_top`'s
  spread. Divergence means the reconstruction is wrong.

---

## 9. Phasing

| Phase | Scope | Gate |
|---|---|---|
| **L0** ✅ silver | `book_l2` schema + DeepBook adapter (snapshots only — no reconstruction needed) | DeepBook silver populated; `gold/depth` reproduces the §1 hand-measured depth profile |
| **L1** ✅ silver | `quote_ladder` + Aftermath adapter | Router rungs in silver; `gold/depth` shows both sources; DeepBook-vs-router crossover visible |
| **L2** | `gold/depth` job + catalog views + timers | 7 consecutive days; quality gates green |
| **L3** ✅ silver | Bluefin adapter (diffs + REST snapshots); `seq_gap` still open | Reconstruction correctness harness: replay a day, compare reconstructed BBO against the venue's own ticker stream |
| **L4** | Backfill `bookTicker` (2023-05 → 2024-04) into `book_top` | Historical BBO queryable — the only pre-2026 spread data for SUI |

**L0 and L1 come first deliberately**: they need no reconstruction (every
row is already a snapshot or a quote), they unblock the exercise-cost half
of the execution model, and they answer the capacity-ceiling question that
doc 07 §9 flagged as the binding constraint. Bluefin's diff stream (L3) is
harder and unblocks the hedge-cost half.

---

## 10. Open questions

1. ~~**Bluefin snapshot cadence**~~ — resolved (SO-446): partial depth
   is top-20 at most, so it never could serve; the REST `/v1/exchange/depth`
   poller at 60 s is the snapshot source (§3 decisions).
2. **Depth truncation** — DeepBook gives 50 levels/side (`depth=100`
   max, `depth=200` errors). At the $1M rung that may be the whole visible
   book, so `exhausted` will be common. Decide whether to record the
   shortfall size or just the flag.
3. **Mid reference for `slippage_bps`** — same-venue mid, or a
   cross-venue consolidated mid? Same-venue is simpler and probably right;
   cross-venue would conflate basis with slippage.
4. **Is `book_l2` worth keeping at full fidelity forever?** It is the
   first silver table large enough for the question to matter. Bronze is
   sacred regardless, so downsampling silver is reversible.
5. **Second market?** Everything above is SUI-only. BTC-PERP on Bluefin
   would cost little and give a liquid control for validating the
   reconstruction harness.
