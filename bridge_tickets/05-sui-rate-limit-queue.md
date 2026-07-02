# 05 — Sui Locker: rate-limit overflow queue (retrofit)

**Status (2026-07-01): DONE — code complete, all tests green.**
- `enforce_rate_limit` (which aborted with code 7) replaced by `within_rate_limit`
  returning a bool; `apply_inbound` now delivers when within cap, else enqueues a
  `QueuedTransfer { recipient, wire_amount, unlock_at_ms }` in a `Table<u64, _>` and
  emits `TransferQueued` — **never reverts**; the message is still consumed at the Inbox.
- `claim(locker, id, clock, ctx)` — permissionless after `unlock_at_ms`, releases/mints
  the exact amount, emits `TransferClaimed`; pause-gated; claims don't consume budget.
  `unlock_at_ms` = `window_start_ms + rate_limit_window_ms` (window end at enqueue).
- Shared `deliver` helper (escrow take / mint) used by both the immediate and claim paths.
- Views `is_queued` / `queued_transfer`; error 7 retired, added `EStillLocked=11`,
  `EUnknownQueueEntry=12`. Behavior now matches the EVM Locker's queue semantics.
- **Tests — 13/13 Sui locker:** queue-then-claim happy path (verifies unlock time, that
  only the in-cap transfer delivered, and the claim releases the queued one),
  claim-before-unlock (11), unknown-entry (12), paused-claim (3), plus the prior 9.
- Note (as flagged): adding struct fields is upgrade-incompatible → the live Sui Locker
  needs a **fresh publish** of this package. **Done 2026-07-01** — this queue-enabled locker
  package (`0x3ef9…`) is the live Locker instance used in ticket 04's round trip (see
  `DEPLOYMENTS.md`).

---


**Spec:** bridge-spec.md §3.5 — "Rate-limit overflow queues; it never reverts."
**Why:** `locker.move` `enforce_rate_limit` aborts with `ERateLimitExceeded` (locker.move:284). An over-limit inbound transfer therefore strands the user's funds at source until the window resets, and relayers burn gas on retries. The EVM Locker (ticket 03) ships the queue from day one; this retrofits the Sui side to match.

## Scope

- Replace the abort path in `apply_inbound`: when `window_used + amount > cap`, record `QueuedTransfer { recipient, wire_amount, unlock_at_ms }` (Table keyed by a counter, or dynamic-field objects) and emit `TransferQueued`. Delivery always succeeds; only the payout is delayed. The message is still consumed at the Inbox — that's the point.
- `claim(locker, queued_id, clock, ctx)` — **permissionless** after `unlock_at_ms`; releases escrow / mints to the recorded recipient; emits `TransferClaimed`. Claims do not consume rate-limit budget (the delay itself was the control), matching NTT semantics.
- `unlock_at_ms` = window end at enqueue time (`window_start_ms + rate_limit_window_ms`).
- Views: `queued(locker, id)`, count. Admin: none new — pause already blocks `claim` via the existing `paused` check (add the assert to `claim`).
- Update the module doc + spec cross-refs; keep `ERateLimitExceeded` error code removed or repurposed deliberately (breaking change to error surface is fine pre-audit).

## Compatibility note
Adding fields to `Locker<T>` / new structs is **not** upgrade-compatible for existing struct layouts — expect a fresh locker publish. Sequence this with ticket 01's republish (one coordinated redeploy) if both are pending; otherwise plan a second locker publish + peer re-wiring.

## Verify (exit criteria)
- Move tests: under-limit passes; over-limit enqueues (does NOT abort) and delivery still marks the message consumed; `claim` before unlock aborts; after unlock releases the exact amount; multiple queued entries; paused locker blocks claim.
- M2 exit test (spec §8): scripted round trip including "a rate-limited transfer that queues and later claims."
- Parity: semantics match the EVM Locker queue (shared scenario vectors).

**Depends on:** 03 (parity target; or land independently if 03 slips). **Blocks:** M2 exit criteria.
