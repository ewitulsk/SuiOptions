# solana-auth-service (`services/solana-auth-service`)

Clone of `auth-service` with Solana wallet-signature login. Issues admin JWTs
gating solana-token-info's public mutations.

## Design decision: separate service, separate JWT domain

Alternative considered: add a `/solana/*` route family to the existing
auth-service (the JWT/challenge/allowlist core is chain-agnostic). Rejected to
follow the port's one-rule ("every Solana service is a new service alongside
the Sui variant") and to keep secrets/allowlists independent — a Sui admin key
compromise shouldn't mint tokens valid for Solana services and vice versa.
Cost: a second tiny container + one more JWT secret. Revisit by merging later
if the duplication annoys; the code is ~600 lines.

## What stays identical

- Two ports: public 9007 (`/challenge`, `/login`, `/refresh`), internal 9008
  (`/verify`). Hand-rolled HS256 JWT, `Claims { sub, ip, iat, exp }`,
  stateless tokens, in-memory single-use challenge store (TTL 300 s), same-IP
  refresh with `iat + refresh_max_secs` bound, allowlist-only authorization.
- Config keys: `environment`, `public_bind_addr`, `internal_bind_addr`,
  `allowed_origins`, `admin_addresses`, `token_ttl_secs`, `refresh_max_secs`,
  `challenge_ttl_secs`. Secret: `[auth] jwt_secret`
  (`options/<env>/solana-auth-service`).
- Consumers verify via the existing `crates/auth-client` (it's just HTTP to
  `/verify` — chain-agnostic), pointed at this service's internal port.

## What changes (`solana_sig.rs` replaces `sui_sig.rs`)

Solana wallets (`wallet-adapter` `signMessage`) produce a detached **ed25519
signature over the raw message bytes**; the address IS the base58-encoded
ed25519 public key. Verification:

1. Decode `bytes` (base64) → must exactly equal an issued, live challenge
   (single-use consume — unchanged).
2. Decode `signature` (base64, 64 bytes) and `pubkey` (base58 — supplied
   explicitly in the login request since Solana signatures don't embed it).
3. `ed25519_dalek::VerifyingKey::verify(message, sig)`.
4. Recovered address = the base58 pubkey; check against `admin_addresses`
   (exact string match, no normalization).

Login request becomes `{ "signature": b64, "bytes": b64, "pubkey": base58 }`.
Challenge message text: `"SuiOptions admin login (solana)\nnonce: <hex>"` —
distinct prefix so a signature for one chain's challenge can never be replayed
against the other service.

Note: some wallets (Ledger via Phantom) wrap messages in the Solana off-chain
message envelope (`\xffsolana offchain`). v1 verifies raw-bytes only —
identical to what `@solana/wallet-adapter` does for hot wallets; document the
Ledger limitation in the frontend guide.

## Verification

- Unit: round-trip test signing a challenge with `ed25519-dalek` and logging
  in; wrong pubkey/signature/allowlist rejection paths; refresh IP binding.
