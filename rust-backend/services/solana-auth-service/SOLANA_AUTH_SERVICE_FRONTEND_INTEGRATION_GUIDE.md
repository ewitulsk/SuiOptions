# solana-auth-service — Frontend Integration Guide

Admin login with a Solana wallet. Challenge-response: fetch a one-time message,
sign it with `wallet-adapter`'s `signMessage`, exchange the signature for a
short-lived JWT. The JWT gates admin mutations on other Solana services (they
verify it against this service's internal `/verify` route).

Base URL (public, via nginx): `/{env}/solana-auth/` → service port 9007.
All bodies are JSON.

## Flow

```
GET /challenge  ──►  { message }
        │
        ▼  wallet.signMessage(utf8(message))
POST /login     ──►  { token, address, expires_in }
        │
        ▼  (before refresh window closes)
POST /refresh   ──►  { token, address, expires_in }
```

### 1. `GET /challenge`

Response:

```json
{ "message": "SuiOptions admin login (solana)\nnonce: 3f9a…64 hex chars…" }
```

- Single-use and expires after 5 minutes (`challenge_ttl_secs`). Fetch a fresh
  one for every login attempt; a reused or expired message is rejected with
  400 `unknown or expired challenge`.
- Sign the message text **exactly as returned** — byte-for-byte.

### 2. Sign with wallet-adapter

```ts
import { useWallet } from "@solana/wallet-adapter-react";

const { publicKey, signMessage } = useWallet();

const { message } = await (await fetch(`${AUTH_URL}/challenge`)).json();

const messageBytes = new TextEncoder().encode(message);
const signature = await signMessage(messageBytes); // Uint8Array, 64 bytes
```

`signMessage` produces a detached ed25519 signature over the **raw message
bytes** — this is exactly what the service verifies.

### 3. `POST /login`

```ts
const toB64 = (b: Uint8Array) => btoa(String.fromCharCode(...b));

const res = await fetch(`${AUTH_URL}/login`, {
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify({
    signature: toB64(signature),      // base64 of the 64-byte signature
    bytes: toB64(messageBytes),       // base64 of the signed message bytes
    pubkey: publicKey.toBase58(),     // base58 address == ed25519 pubkey
  }),
});
```

Unlike the Sui service, `pubkey` is a separate field: Solana signatures don't
embed the public key. The service verifies the signature against that pubkey;
supplying someone else's address cannot succeed without their key.

Success (200):

```json
{
  "token": "eyJhbGciOiJIUzI1NiIs…",
  "address": "4Nd1mBQtrMJVYVfKf2PJy9NZUZdTAsp7D4xWLs4gDB4T",
  "expires_in": 3600
}
```

Errors:

| Status | Meaning |
|---|---|
| 400 | `bytes` not base64 / not UTF-8, or challenge unknown/expired/reused |
| 401 | signature or pubkey malformed, or signature doesn't verify |
| 403 | signature valid but address not on the admin allowlist |

Send the token on admin requests as `Authorization: Bearer <token>`.

### 4. `POST /refresh`

```ts
const res = await fetch(`${AUTH_URL}/refresh`, {
  method: "POST",
  headers: { Authorization: `Bearer ${token}` },
});
```

Returns the same `{ token, address, expires_in }` shape. Semantics:

- No re-signing needed; the current (even just-expired) token is enough.
- Must come from the **same IP** the token was issued to.
- The session is bounded: refresh works until `iat + refresh_max_secs`
  (24 h from the original login), after which the wallet must sign in again.
  Each refresh slides `exp` forward but keeps the original `iat`.
- 401 on any failure (bad token, window elapsed, IP mismatch) — send the user
  back through the challenge/login flow.

## Notes and limitations

- **Ledger (v1 unsupported):** hardware wallets signing through the Solana
  off-chain message envelope (`\xffsolana offchain…`, e.g. Ledger via Phantom)
  wrap the message before signing, so the signature is not over the raw bytes
  and login will fail with 401. v1 verifies raw-byte signatures only — use a
  hot wallet for admin login.
- The challenge prefix `SuiOptions admin login (solana)` is deliberately
  distinct from the Sui auth-service's, and the two services use different JWT
  secrets: tokens and signatures are never valid across chains.
- Tokens are stateless; there is no logout endpoint. Drop the token client-side
  and it dies at `exp`.
