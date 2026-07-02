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
| `POST /sign_message` | `{message: CrossChainMessage}` → verify §5.4 boundary, then sign with the destination family's scheme (Ed25519 for Sui, ECDSA for EVM). Returns `{message_hash, envelope}`. |
| `GET /group_keys` | The Ed25519 pubkey + ECDSA address + ids to register on-chain via `registerGroupKey`. |
| `GET /get_attestation` | **M1 stub** — real Nautilus attestation is M3/M4. |
| `GET /health`, `GET /metrics` | liveness + Prometheus. |

**Admin (port 3001, localhost-only in prod):** Seal key-load, share
provisioning, and DKG (`/admin/*`) — all return `501` until M3.

## The security boundary (§5.4)

`/sign_message` will only sign a message that the source Outbox committed at
finality. That check is the [`SourceVerifier`](src/verifier.rs) trait:
- `trust_all` — **DEV ONLY**, skips the check (logs a warning).
- `rpc` — verify against a source-chain RPC view. **Not built at M1** — this is
  the signer's remaining M1 work, and the reason the service isn't production
  safe yet on its own.

## Tests

`tests/sign.rs` drives the real router and asserts `/sign_message` returns the
exact signature the on-chain `known_digest_vector` tests expect — end-to-end
proof the service interoperates with the Sui Inbox.
