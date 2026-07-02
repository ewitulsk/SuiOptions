# 06 — Async signing API (submit → poll by message_hash)

**Status (2026-07-01): DONE — code complete, tested (unit + live HTTP).**
- `POST /sign_requests` → `202 {message_hash, status, envelope?}`, idempotent per hash
  (duplicate coalesces onto the existing session via a Pending marker);
  `GET /sign_requests/:hash` → `{status: pending|signed, envelope?}` or 404. `/sign_message`
  removed. (Route uses axum-0.7 `:param` syntax.)
- Session store (`sessions.rs`): keyed by hash, TTL-evicts terminal sessions, bounded map
  (sheds load → 503), verify-failure abandons (not cached → retryable). Unit-tested.
- DoS guardrails (§5.3): **verify-before-admit** (uncommitted → 422 at the door, never
  queued), in-flight dedup, bounded map, per-IP fixed-window rate limit (`ratelimit.rs`,
  wired via `ConnectInfo`). Config knobs added (ttl/cap/rate).
- Relayer `signer_client.rs`: submit-then-poll behind the unchanged `RemoteSigner::sign`,
  so `relay.rs` is untouched and the M3 MPC turn-on needs no client edit.
- **Tests:** signer-service 22 lib (5 session + 2 ratelimit + verifier/probe) + 5 integration
  (completes+pollable, duplicate-coalesces, uncommitted-422-at-door, unsupported-family-400,
  unknown-hash-404); relayer 9. **Live HTTP smoke:** ran the service, POST→202 signed with
  the exact known-vector signature, GET→200 same, unknown→404.

---


**Spec:** bridge-spec.md §5.3
**Why:** the signer exposes a synchronous `POST /sign_message` (router.rs:23). FROST/GG20 at k > 1 are multi-round protocols across nodes — a synchronous request/response API cannot survive M3 (ticket 09). The spec resolved to design the poll model now so the interface doesn't break when MPC turns on. Also hardens the public DoS surface.

## Scope

### Endpoints (replace `/sign_message`)
- `POST /sign_requests` `{message}` → `202 {message_hash, status}`. Idempotent **per message_hash**: one signing session per digest, ever; duplicate submissions coalesce onto the existing session.
- `GET /sign_requests/{message_hash}` → `{status: "pending" | "signed" | "rejected", envelope?, reason?}`.
- Keep `/group_keys`, `/get_attestation`, `/health` unchanged.

### Session store
In-memory map `message_hash → SessionState` with TTL eviction for terminal states. At M1 the "session" is trivial (verify → sign inline, likely completing before the first poll) — the point is the **interface**, which M3 swaps internals under.

### DoS guardrails (§5.3)
- Run the ticket-02 source-commitment verification **before** admitting a session — anything not committed on a registered Outbox is rejected at the door (422), never queued.
- In-flight dedupe by hash (free with idempotency); bounded session map; per-source-IP rate limit on `POST`.

### Relayer update
`signer_client.rs` (`RemoteSigner` trait impl): submit, then poll with backoff until `signed`/`rejected`. The trait signature can stay `async fn sign(&self, m) -> Result<SignatureEnvelope>` — polling is an implementation detail, so `relay.rs` is untouched.

### Migration
Remove `/sign_message` in the same change (grep-able, only the relayer consumes it). Update both READMEs + config examples + `tests/sign.rs`.

## Verify (exit criteria)
- Unit: idempotency (two concurrent POSTs of the same message → one session, both get the same envelope); rejected-at-door for uncommitted messages; TTL eviction; poll state machine.
- Integration: relayer end-to-end through the new API on the live smoke path.
- Load sanity: N duplicate submissions cause exactly one verify + one sign.

**Depends on:** 02 (verify-before-queue is the admission gate). **Blocks:** 09 (MPC needs the async surface).
