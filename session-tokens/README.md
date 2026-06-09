# SIWS Session Keys for Sui

Reference implementation of [`sui-siws-session-key-spec.md`](./sui-siws-session-key-spec.md):
authenticate to a Sui dApp with a Solana wallet, mint a scoped, expiring,
revocable on-chain `SessionCap` to a browser-generated temporary key, and let
that key act on the user's behalf — within enforced limits — without signing
every transaction.

## Layout

| Folder | What |
|--------|------|
| [`contracts/`](./contracts) | Move package — `registry`, `account`, `session`, `app_example`, the canonical `message` serializer, `errors`. Built + unit-tested (`sui move test`). |
| [`sdk/`](./sdk) | TypeScript browser SDK — non-extractable WebCrypto session key, `createSession` / `execute` / `status` / `revoke` / `restoreSession`, local + http sponsor clients. The serializer is byte-exact with the Move side (pinned both ways). |
| [`demo-frontend/`](./demo-frontend) | Vite + React + dapp-kit demo (same stack as `../frontend`) exercising the whole flow with Phantom. |

## The three trust boundaries

1. **Solana key** = root of identity. Used rarely (sign-in, renew, revoke).
2. **SessionCap / temp Sui key** = scoped, expiring, revocable delegate.
3. **Sponsor / relayer** = pays gas, *cannot move user funds* (it never holds
   the `SessionCap`).

## Build order

```bash
# 1. Contracts (get the serializer byte-exact first — highest-risk integration).
cd contracts && sui move test && sui move build

# 2. SDK (serializer parity test mirrors the Move reference vectors).
cd ../sdk && npm install && npm test && npm run build

# 3. Demo (point .env.local at the published package — see demo-frontend/README).
cd ../demo-frontend && npm install && npm run dev
```

## Highest-risk seam: the canonical message

`contracts/sources/message.move` and `sdk/src/message.ts` must produce identical
bytes — the contract rebuilds the signed message from checked args and verifies
the Solana ed25519 signature against it, so any divergence breaks sign-in. Both
are pinned against the same reference vectors in their test suites
(`session_tests.move` / `message.test.ts`).

## Status / notes

- The demo's sponsor runs **in-browser** for convenience; production should run
  it as a backend relayer (the SDK ships `HttpSponsorClient` for that shape).
- Nonces are tracked in the global `Registry` (matches the spec's §1.5 code).
  For higher throughput, shard the registry or move nonces into the per-user
  `Account` (spec §1.3).
- Single `Coin<T>` per Account; allowlist uses full `pkg::module::function`
  selectors (spec §5 recommendations).
