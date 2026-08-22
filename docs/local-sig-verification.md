# Auth-service: fully local verification for ALL Sui signature schemes

## Context

`sui_sig.rs` hand-rolls personal-message verification for plain keypairs only
(flags 0/1/2: ed25519, secp256k1, secp256r1) and rejects everything else —
zkLogin (flag 5), MultiSig (flag 3), Passkey (flag 6). Goal: replace the whole
verifier with the validator-grade stack from `sui-types`, verifying every
scheme **locally** — no remote `verifySignature` delegation.

Verified against the pinned deps (sui git rev `2f5992f`, fastcrypto `5f87e04`,
both already in the workspace tree):
- `sui_types::signature::{GenericSignature, VerifyParams}` +
  `GenericSignature::verify_authenticator(intent_msg, author, epoch, params, cache)`
  — the exact entry point validators use; dispatches all five schemes.
- `SuiAddress: TryFrom<&GenericSignature>` — address derivation for every scheme.
- `shared_crypto::intent::{Intent, IntentMessage, PersonalMessage}` — replaces
  the hand-rolled blake2b/uleb/intent code.
- `fastcrypto_zkp::bn254::zk_login::{OIDCProvider, JwkId, JWK, fetch_jwks}` —
  provider registry with JWK endpoints baked in (Google/Twitch/Facebook/Apple/…)
  and an async fetch helper.
- `fastcrypto_zkp::bn254::zk_login_api::ZkLoginEnv` (`Prod` for testnet+mainnet).
- `sui_types::signature_verification::VerifiedDigestCache<ZKLoginInputsDigest>`
  — LRU of already-verified Groth16 inputs.

Honest boundary: the **crypto** is all local, but zkLogin verification has two
*data* inputs that are inherently external — the OIDC providers' current JWKs
(fetched from the providers' own endpoints, rotating) and the current Sui epoch
number (chain state, via the existing `sui_graphql_url`). These are public
inputs, not delegated trust: a wrong value makes verification fail, never pass.

## Phase 1 — swap the verifier core (no new I/O)

- Deps: add `sui-types` + `shared-crypto` + `fastcrypto-zkp` + `im` to
  auth-service. **Trap:** fastcrypto-zkp must resolve to the same git rev
  sui-types pins or the `JwkId`/`JWK` types won't unify — depend with the
  identical git spec and confirm with `cargo tree -i fastcrypto-zkp` (one node).
- Rewrite `sui_sig.rs`:
  - parse `GenericSignature::from_bytes`,
  - build `IntentMessage::new(Intent::personal_message(), PersonalMessage { message })`,
  - derive the author via `SuiAddress::try_from(&sig)` for flags 0/1/2/3/6;
    for zkLogin take the client-claimed address (Phase 3) — `verify_claims`
    proves the signature binds to it (the validator pattern; avoids the
    padded/legacy-unpadded derivation duality),
  - call `verify_authenticator(...)`; return the canonical `0x…` address.
  - Delete the hand-rolled blake2b/uleb/flag-parsing code.
- Phase-1 `VerifyParams`: empty JWK map + empty provider list (zkLogin still
  rejected, but now with a precise error), `validate_zklogin_public_identifier:
  true`, `verify_legacy_zklogin_address: false`. Epoch arg = 0 until Phase 2
  (it is only consulted by epoch-bounded schemes).
- Deliberate behavior expansion to sign off on: **MultiSig and Passkey wallets
  can now log in** (they still have to pass allowlist/AdminCap like everyone).
  A multisig's address is its own — no member-address confusion is possible.

## Phase 2 — zkLogin data plumbing

- `jwks.rs`: source the JWKs from the chain's consensus-agreed registry —
  `object(0x7)` (`authenticator_state::AuthenticatorState`) →
  `dynamicFields` → `AuthenticatorStateInner.active_jwks`, one GraphQL query
  on the existing `sui_graphql_url` (verified live: 66 `(iss,kid)` entries,
  fields map 1:1 onto fastcrypto's `JwkId`/`JWK`). This gives exact validator
  parity (we accept precisely what the chain accepts) and one fetch for every
  provider instead of a per-provider fan-out. Background tokio task refreshes
  every `jwk_refresh_secs` (default 900) into an `RwLock<ImHashMap<JwkId,
  JWK>>`; a login with an unknown `kid` forces one refresh and retries once
  (rotation races — a rotated key reaches 0x7 minutes after the provider).
  Provider-direct `fetch_jwks(provider, &client, true)` stays as a documented
  fallback. Empty map → zkLogin logins fail closed; classic wallets and the
  static list are unaffected.
- `epoch.rs`: current epoch via GraphQL `{ epoch { epochId } }` — batched into
  the SAME request as the 0x7 read (one aliased query serves both), cached
  with a ~10-min TTL (testnet epochs are 24 h; the staleness window means a
  sig could be accepted up to TTL past its `max_epoch` boundary — irrelevant
  next to the 1 h JWT TTL). Never fetched → zkLogin fails closed.
- Assemble the real `VerifyParams`: JWK map, providers,
  `zk_login_env` from config (`prod` = testnet+mainnet verifying key),
  `zklogin_max_epoch_upper_bound_delta: None` (that bound protects tx signing;
  login only needs `current ≤ max_epoch`), `accept_zklogin_in_multisig: true`.
- One process-wide `Arc<VerifiedDigestCache<ZKLoginInputsDigest>>` (its `new`
  wants prometheus `IntCounter`s — register three, or use the unit-value impl).
- Config (all optional; unset in dev → zkLogin off):
  `zklogin_providers = ["google"]`, `zklogin_env = "prod"`,
  `jwk_refresh_secs = 900`. Reuses `sui_graphql_url` from SO-422.
- Metrics: `auth_jwk_age_seconds`, `auth_jwk_fetch_errors_total`,
  `auth_epoch_age_seconds`, `auth_logins_total` gains a `scheme` label.

## Phase 3 — API + frontend

- `LoginReq` gains optional `address`. Required when the signature flag is 5
  (400 if absent); for other schemes it is a cross-check (400 on mismatch with
  the derived address). Backward compatible — existing clients keep working.
- Frontend: the shared `useAdminAuth` login call includes
  `currentAccount.address`. One field, one place.
- No nginx/compose/port changes. `refresh` and `/verify` untouched (JWT-based).
  The AdminCap fallback (SO-422) is downstream of signature verification and
  works for zkLogin addresses as-is (ownership query is scheme-agnostic).

## Phase 4 — tests & rollout

- Unit: re-point the existing ed25519 round-trip through the new path; add
  secp256k1/r1 round-trips; a MultiSig fixture; zkLogin fixtures from
  fastcrypto-zkp's test vectors (Mysten test-issuer JWK + proof, run under
  `ZkLoginEnv::Test`) — exercises the full Groth16 path without a prover.
- Live staging: classic-wallet regression login, unlisted-wallet 403, and a
  real zkLogin (Enoki) wallet login through `/admin`.
- Rollout: one PR, `Deploy staging` with `only_services=["auth-service"]`.
  Build-time note: sui-types swells the auth-service dependency graph — CI
  gha cache scope absorbs it after the first build.

## Risks

1. **fastcrypto rev drift** — the unify check above is the gate; a mismatch is
   a compile error at worst, silent type duplication at best.
2. **Provider endpoint outage / rotation race** — refresh-on-unknown-kid +
   fail-closed; static allowlist + classic wallets unaffected.
3. **Legacy unpadded zkLogin addresses** — kept off; flip
   `verify_legacy_zklogin_address` consciously if an old account surfaces.
4. **Epoch staleness** — bounded by cache TTL; acceptable for login.
5. Future sui rev bumps change these APIs — but the dep is pinned by the same
   workspace that publishes contracts, so it moves deliberately.
