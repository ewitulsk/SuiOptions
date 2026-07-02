# 06 — Async signing API (submit → poll by message_hash)

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
