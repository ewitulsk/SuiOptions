# bridge-signer-service

The Layer 1 **signer node** ([bridge-spec.md §5](../../../bridge-spec.md)). Wraps
[`bridge-signer`](../../crates/bridge-signer) behind the §5.3 HTTP surface and
enforces the §5.4 security boundary before signing.

At **M1** it is a single-party signer ("1-of-1"): keys come from config seeds.
The aggregated signature is on-chain-indistinguishable from a later k-of-n one,
so nothing changes on-chain when threshold signing (M3) turns on.

```
cargo run -p bridge-signer-service -- --config config.toml
```

## Endpoints

**Public (port 3000):**
| Route | Purpose |
|-------|---------|
| `POST /sign_requests` | `{message}` → verify §5.4 boundary, then sign (Ed25519 for Sui, ECDSA for EVM). **Idempotent per message hash**; returns `202 {message_hash, status, envelope?}`. At M1 the 202 already carries `status:"signed"`; the async surface is for M3 MPC. |
| `GET /sign_requests/:hash` | Poll a session → `{message_hash, status: pending\|signed, envelope?}`, or `404` if unknown. |
| `GET /group_keys` | The Ed25519 pubkey + ECDSA address + ids to register on-chain via `registerGroupKey`. |
| `GET /get_attestation` | **M1 stub** — real Nautilus attestation is M3/M4. |
| `GET /health`, `GET /metrics` | liveness + Prometheus. |

DoS guardrails (§5.3): the §5.4 verify runs **before** a session is admitted (an
uncommitted message is `422` at the door, never queued); duplicate in-flight
requests coalesce by hash; the session map is bounded + TTL-evicted; and
`POST /sign_requests` is per-IP rate limited.

**Admin (port 3001, localhost-only in prod):** Seal key-load, share
provisioning, and DKG (`/admin/*`) — all return `501` until M3.

## The security boundary (§5.4)

The signer only signs a message the source Outbox committed at finality. That
check is the [`SourceVerifier`](src/verifier.rs) trait:
- `trust_all` — **DEV ONLY** (rejected unless `environment = "dev"`), skips the check.
- `rpc` — verify against ≥2 independent source-chain RPC providers (the
  [`RpcVerifier`](src/verifier.rs), spec §5.4): the registered Outbox must have
  committed the exact message at the configured confirmation depth.

## Tests

`tests/sign.rs` drives the real router: a `POST /sign_requests` returns the exact
signature the on-chain `known_digest_vector` tests expect (end-to-end proof the
service interoperates with the Sui Inbox), a duplicate POST coalesces onto one
session, and an uncommitted message is `422` at the door with no cached session.
