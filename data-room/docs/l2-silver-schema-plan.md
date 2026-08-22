# L2 depth: silver schema + gold slippage surface

Status: **PLANNED, not started. 2026-08-15.** Promotes spec §13 P4
("depth — deferred") into scope, because the backtesting framework's
execution model has no other input.

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
| Bluefin WS `OrderbookPartialDepthUpdate` | periodic **snapshots** | venue-defined |
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
| `seq` | i64 | venue sequence / update id — ordering **and** gap detection |
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

Row sort for determinism (spec §7): `(ts_recv, seq, side, price)`.
Writer settings from the existing `schema::writer_props()`.

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

- **Bluefin capture must subscribe to `OrderbookPartialDepthUpdate` as
  well as the diff stream.** Diffs alone make a day's partition
  unreconstructable without replaying the previous day, which breaks the
  partition-independence the whole design rests on.
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
| `ts_recv` | i64 ns | |
| `exchange` | dict str | `aftermath`, later others |
| `pair` | dict str | e.g. `SUI-USDC` |
| `side` | dict str | `sell_base` \| `buy_base` |
| `amount_in` | f64 | human units |
| `amount_out` | f64 | human units |
| `route` | str, nullable | protocols traversed, e.g. `Bluefin,Cetus` — diagnostic |
| `src_file`, `src_line` | | lineage |

Slippage is derived, not stored: it needs a reference mid, which is a
gold-layer join. Keep silver a faithful record of what the venue quoted.

Note the response quirk recorded in `sui-collection-plan.md` §1.1b —
Aftermath returns BigInt-style strings with a trailing `n`. The adapter
strips it; a golden fixture must cover it.

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
| `crates/schema/src/lib.rs` | add `CanonicalEvent::{BookL2, QuoteLadder}` variants + `BookL2` / `QuoteLadder` structs; `book_l2_schema()` / `book_l2_batch()`, `quote_ladder_schema()` / `quote_ladder_batch()`; a `BookL2Writer` mirroring `TradesWriter` (per-day, `flush()` per 100k — the batch-memory discipline) |
| `crates/adapters/src/bluefin.rs` | new: `parse()` → `BookL2` events, `route()` → bronze stream names. Golden fixtures from real captures |
| `crates/adapters/src/deepbook.rs` | new: parse the indexer snapshot JSON → `BookL2` with `is_snapshot=true` |
| `crates/adapters/src/aftermath.rs` | new: parse `/trade-route` → `QuoteLadder` (strip trailing `n`) |
| `normalizer/src/ws.rs` | `parse_payload()` + `stream_target()` arms: `("bluefin","book") => ("book_l2", sym)` |
| `normalizer/src/` (new module) | poller-sourced snapshots: DeepBook + Aftermath bronze → silver. These are poller streams, not WS, so they need their own path (compare `deribit.rs`, which does exactly this for chain polls) |
| `gold/src/depth.rs` | new: book replay + ladder walk → `gold/depth` |
| `gold/src/main.rs` | `depth` subcommand |
| `gold/src/gaps.rs` | emit `kind="seq_gap"` |
| `catalog/catalog.sql` | views: `book_l2`, `quote_ladder`, `depth` |
| `deploy/systemd/data-room-normalizer.service` | add the new normalizer invocations |
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
| **L0** | `book_l2` schema + DeepBook adapter (snapshots only — no reconstruction needed) | DeepBook silver populated; `gold/depth` reproduces the §1 hand-measured depth profile |
| **L1** | `quote_ladder` + Aftermath adapter | Router rungs in silver; `gold/depth` shows both sources; DeepBook-vs-router crossover visible |
| **L2** | `gold/depth` job + catalog views + timers | 7 consecutive days; quality gates green |
| **L3** | Bluefin adapter (diffs + snapshots) + `seq_gap` | Reconstruction correctness harness: replay a day, compare reconstructed BBO against the venue's own ticker stream |
| **L4** | Backfill `bookTicker` (2023-05 → 2024-04) into `book_top` | Historical BBO queryable — the only pre-2026 spread data for SUI |

**L0 and L1 come first deliberately**: they need no reconstruction (every
row is already a snapshot or a quote), they unblock the exercise-cost half
of the execution model, and they answer the capacity-ceiling question that
doc 07 §9 flagged as the binding constraint. Bluefin's diff stream (L3) is
harder and unblocks the hedge-cost half.

---

## 10. Open questions

1. **Bluefin snapshot cadence** — does `OrderbookPartialDepthUpdate` fire
   often enough to give several snapshots per UTC day? If not, the
   collector needs its own periodic resync poll. Blocks L3.
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
