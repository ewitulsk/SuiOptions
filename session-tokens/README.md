# Session Tokens (Experimental) — SIWS / SIWE Session Keys for Sui

An experiment in **wallet-rooted session keys** for Sui. A user signs in **once**
with a wallet they already have — a Solana wallet (Sign-In With Solana) or an
Ethereum wallet (Sign-In With Ethereum, [EIP-4361](https://eips.ethereum.org/EIPS/eip-4361))
— which mints a scoped, expiring, revocable on-chain `SessionCap` to a
browser-generated ephemeral key. That ephemeral key then acts on the user's
behalf **within enforced limits and without prompting for a signature on every
transaction**, while a sponsor pays the gas.

This is differentiated for the **cross-chain identity** case: a user whose root
identity is a Solana or Ethereum key can drive a Sui dApp without ever holding
SUI or signing each Sui transaction.

Full design: [`sui-siws-session-key-spec.md`](./sui-siws-session-key-spec.md).

> **Status:** experimental, and now **integrated into the options protocol**:
> `contracts/` (options_protocol) depends on this package and defines
> `_with_session` twins of every user-facing entrypoint, the gas station
> sponsors the session PTB shapes, and the frontend's account dropdown owns
> the whole session lifecycle. The on-chain testnet deployment below predates
> the v2 (multi-asset, per-type-limit) rework — redeploy via
> `deployment-manager --deploy-session` before using it.

---

## The idea in one picture

```
  root wallet (Phantom / MetaMask)         browser
  ───────────────────────────────          ───────────────────────────
  sign ONE message  ──────────────────►    ephemeral Sui keypair
  (SIWS or SIWE/EIP-4361)                   (non-extractable WebCrypto)
          │                                          │
          │ signature                                │ sponsored tx
          ▼                                          ▼
  ┌──────────────────────────────────────────────────────────────┐
  │  Sui Move package                                             │
  │  registry  → Account (shared, per-user, holds Coin<T>)        │
  │  verify root sig (ed25519 | secp256k1 ecrecover)             │
  │  mint SessionCap → ephemeral key                             │
  │  app entrypoints require &SessionCap, enforce caps           │
  └──────────────────────────────────────────────────────────────┘
          ▲ auto-signed app txs (ephemeral key signs, sponsor pays gas)
```

### Three trust boundaries (keep them straight)

1. **Root wallet key** (Solana / Ethereum) — the root of identity. Used rarely:
   sign-in, renew, revoke.
2. **`SessionCap` / ephemeral Sui key** — a scoped, expiring, revocable
   delegate. Caps bound the *magnitude and duration* of any loss.
3. **Sponsor / relayer** — pays gas and co-signs. **Cannot move user funds**: it
   never holds the `SessionCap`.

---

## Layout

| Folder | What |
|--------|------|
| [`contracts/`](./contracts) | Move package. `registry` (identity → account map + nonce set), `account` (per-user shared **multi-asset** treasury + per-(cap, coin-type) spend ledger), `session` (cap mint + verify + revoke + the **public** `authorize` / `authorize_spend<T>` target packages call from their own cap-gated entrypoints), `message` (canonical SIWS serializer, `siws-session-v2` with signed per-type limits), `siwe` (EIP-4361 builder + EIP-191 + `ecrecover`), `app_example`, `errors`. `sui move test` (14 tests, incl. the expire→re-sign-in→same-account pin). |
| [`../frontend/siws-session-sdk/`](../frontend/siws-session-sdk) | TypeScript browser SDK (lives under `frontend/` so the app's single `npm install` resolves its imports — it's consumed from source). Non-extractable WebCrypto session key, `createSession` (Solana) / `createSessionEth` (Ethereum) / `execute` / `status` / `revoke` / `restoreSession`. External gas stations plug in through `GasStationAdapter` + `GasStationSponsorClient` (the co-sign-the-sponsor's-exact-bytes pipeline lives in the SDK once); `suiOptionsGasStation` ships the adapter for `rust-backend/services/gas-station`. Both serializers are byte-exact with the Move side, pinned both ways (`gen-siwe.mjs` regenerates the shared vectors). |
| [`demo-frontend/`](./demo-frontend) | Vite + React + dapp-kit demo (same stack as `../frontend`). Connect Phantom **or** MetaMask, open a session, fund the account, auto-signed withdraws, sweep stray coins, revoke, with Suiscan tx toasts. |

**The real integration** lives in the main app: `contracts/` defines
`_with_session` twins (account create/withdraw/key-rotate, write, buy,
exercise, redeem, burn-expired) that source funds from — and settle outputs
into — the user's options `Account` custody under the cap's per-type limits;
`rust-backend` gains `session_*` gas-station templates and
`deployment-manager --deploy-session`; `frontend/src/session/` + the nav-bar
account dropdown own sign-in/restore/fund/withdraw/revoke.

### Run it

```bash
# 1. Contracts — get the byte-exact serializers right first (highest-risk seam).
cd contracts && sui move test && sui move build

# 2. SDK — its serializer-parity tests mirror the Move reference vectors.
cd ../../frontend/siws-session-sdk && npm install && npm test

# 3. Demo — point .env.local at the deployed package (see demo-frontend/README.md),
#    fund the sponsor address it shows from the testnet faucet, then:
cd ../demo-frontend && npm install && npm run dev   # http://localhost:5174
```

---

## Multi-wallet: SIWS and SIWE

The session-key architecture is identical for both roots; only the **root
signature verification** differs.

| | Solana (SIWS) | Ethereum (SIWE / EIP-4361) |
|---|---|---|
| Curve | ed25519 | secp256k1 (ECDSA) |
| Identity stored in `Account.owner_pk` | 32-byte pubkey | 20-byte address |
| Signed message | our canonical `siws-session-v1` format | the canonical EIP-4361 message |
| Wallet signing | raw bytes | `personal_sign` (EIP-191 prefix + keccak) |
| On-chain verify | `ed25519_verify(sig, pubkey, msg)` | `secp256k1_ecrecover(sig, msg, keccak)` → derive address → compare |
| Sign-in entrypoint | `verify_and_open_session` | `verify_and_open_session_eth` |

The scheme is inferred from `owner_pk` length (32 = ed25519, 20 = eth), so a
single `Account` type and `SessionCap` serve both — **no struct change** was
needed to add Ethereum (an upgrade-safe consideration; see below).

### Highest-risk seam: the canonical message

The contract **never trusts a caller-supplied message blob.** It rebuilds the
exact signed bytes from on-chain/checked values and verifies against that. So
the SDK and the Move package must serialize **byte-for-byte identically**, or
sign-in silently breaks.

- SIWS: `contracts/sources/message.move` ↔ `../frontend/siws-session-sdk/src/message.ts`
- SIWE: `contracts/sources/siwe.move` ↔ `../frontend/siws-session-sdk/src/siwe.ts`

Each pair is pinned against the **same reference vectors** in both test suites
(`*_tests.move` / `*.test.ts`). The SIWE vector uses a **real** signature so the
test exercises the full EIP-191 + `ecrecover` + keccak address-derivation path
(including EIP-55 checksum casing) against Sui's native crypto.

---

## Design decisions worth knowing

These came up while building and shaped the implementation:

### The `Account` is a keyless Move *object*, not a Sui *address* — on purpose

In Sui, object-ids and addresses share the same 32-byte space, which is a
footgun: you can "send SUI to the Account's id" from a normal wallet and the
coin lands at an **address with no private key** (the object id reinterpreted as
an address) — stranded, and invisible to the contract because the spendable
balance lives in the object's `funds: Balance<T>` field, not as owned coins.

We keep the `Account` a keyless object **deliberately**. The whole point of the
design is that funds move only through cap-checked entrypoints, driven by the
contract — never by a private key for the account itself. Making it a "real"
signing address would re-introduce "sign every withdrawal," defeating the
system.

**Funding the account, correctly:**
- **`deposit(account, coin)`** — the normal path (one step, funds land in
  `funds`). The demo's "Fund Account" button.
- **`receive(account, Receiving<Coin<T>>)`** — a recovery/robustness path using
  Sui's `transfer::Receiving`: it reclaims coins that were wallet-transferred to
  the object id (`AddressOwner`) into `funds`. The demo's **Sweep** button
  detects such coins and batches a `receive` per coin into one tx. This was
  added after a real "I sent SUI to the object id and it vanished" moment.

### Sponsor allowlist ≠ on-chain cap allowlist (two separate gates)

- The **`SessionCap.allowed`** list is enforced **on-chain** in
  `session::authorize` and gates which app functions a cap may call (full
  `pkg::module::function` selectors).
- The **sponsor's `allowedTargets`** is a separate, defense-in-depth gate on
  which calls the *relayer will pay gas for* — and it must include the session
  entrypoints (`verify_and_open_session[_eth]`, `revoke_all[_eth]`), not just
  the app calls, since the sponsor co-signs all of them.

### Ephemeral key handling

Generated as a **non-extractable WebCrypto Ed25519** key — the private key never
appears in JS memory; signing delegates to `crypto.subtle.sign`. Reload
survival persists only the non-extractable `CryptoKeyPair` handle in IndexedDB
(never a raw secret). A software-key fallback exists where WebCrypto Ed25519 is
unavailable, but those sessions intentionally do not survive reload.

---

## Deployment

Deployed to **Sui testnet**. The `Registry` is a shared object created at
publish and **stable across upgrades**; the package id changes on each upgrade.

| | Object id |
|---|---|
| **Package (current)** | `0x9155b5b22019d2d17ae413933e315857417bb69c1e6fcafc844f2480694e5514` |
| **Registry** (shared) | `0x9cfda308339a3825475f0ebe209a0f33b2b62621cadf876ff3f698c5df9b5bab` |
| `UpgradeCap` | `0x30f046c605d14a6ac662e6e7e654a809af40b15ac5e345af0e843292f8edb5d4` |

Upgrade history:
1. `0x240d…a850` — initial publish.
2. `0x525d…f04b` — added `account::receive` (sweep stranded coins).
3. `0x9155…5514` — added EIP-4361 / Ethereum support (`siwe`, `*_eth`). **current**

> **Toolchain gotcha:** testnet's protocol version runs ahead of the Homebrew
> `sui` CLI, which panics on `publish`/`upgrade` (protocol-config mismatch).
> Reads work, but the **upgrades were submitted over RPC** using locally
> compiled bytecode (`sui move build --dump-bytecode-as-base64`) rather than
> `sui client upgrade`. The on-chain Ethereum path was additionally smoke-tested
> live via `devInspectTransactionBlock` with a real (viem) signature.

---

## Known limitations / residual risk

- **Demo sponsor runs in-browser** for convenience (a gas keypair in
  localStorage). Production should run it as a backend relayer — the SDK ships
  `HttpSponsorClient` for exactly that shape.
- **Nonces live in the global `Registry`** (matches the spec's §1.5 code). For
  higher throughput, shard the registry or move nonces into the per-user
  `Account` (spec §1.3).
- **Single `Coin<T>` per Account**; multi-asset would complicate the spend
  ledger.
- **Residual risk:** within an unexpired cap's remaining budget and allowlist, a
  compromised session key can act. Session keys cap the *magnitude and duration*
  of loss — they don't eliminate it. Defaults are conservative (short TTL, low
  per-tx and total caps); dApps can opt into more.
