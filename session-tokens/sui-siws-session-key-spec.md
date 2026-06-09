# SIWS Session-Key SDK for Sui — Implementation Spec

A spec for an SDK that lets a user authenticate to a Sui dApp with a Solana
(or any "Sign In With"-capable) wallet, mints an on-chain `SessionCap` to a
browser-generated temporary Sui key, and lets that key act on the user's
behalf — within enforced limits — without signing every transaction.

---

## 0. System overview

```
┌─────────────┐    1. SIWS sign      ┌──────────────┐
│  Solana     │◄─────────────────────│  Browser SDK │
│  wallet     │   (message w/ temp   │              │
│ (Phantom…)  │    Sui addr, nonce,  │  ephemeral   │
└─────────────┘    expiry, domain)   │  Sui keypair │
       │                             └──────┬───────┘
       │ signature + pubkey + msg           │ 2. submit (sponsored tx)
       ▼                                     ▼
┌──────────────────────────────────────────────────────────┐
│                    Sui smart contracts                     │
│                                                            │
│  Registry (shared)  ──maps──►  Account (shared, per user)  │
│   solana_pk → acct ID          holds Coin<T>, generation,  │
│   consumed nonces              spend ledger                │
│                                                            │
│  verify_and_open_session():                                │
│    - reconstruct msg bytes from on-chain values            │
│    - ed25519_verify(sig, pk, msg)                          │
│    - check nonce unused, expiry, domain                    │
│    - create/lookup Account                                 │
│    - mint SessionCap → temp Sui addr                       │
│                                                            │
│  app entrypoints: require &SessionCap, enforce limits,     │
│    pull from Account                                       │
└──────────────────────────────────────────────────────────┘
       ▲
       │ 3. auto-signed app txs (temp key signs, sponsor pays)
┌──────┴───────┐
│  Browser SDK │
└──────────────┘
```

Three trust boundaries to keep straight:
1. **Solana key** = root of identity. Rarely used (sign-in + renewal + revoke).
2. **SessionCap / temp Sui key** = scoped, expiring, revocable delegate.
3. **Sponsor/relayer** = pays gas, cannot move user funds.

---

## 1. On-chain: Move package

### 1.1 Module layout

```
sources/
  registry.move      // global identity → account map + nonce set
  account.move       // per-user shared treasury + generation counter
  session.move       // SessionCap type, mint, verify, revoke
  app_example.move   // example scoped entrypoint
```

### 1.2 Core types

```move
module siws_session::account {
    use sui::object::{Self, UID, ID};
    use sui::balance::{Self, Balance};
    use sui::table::{Self, Table};

    /// One per user. Shared object. Holds funds + control state.
    public struct Account<phantom T> has key {
        id: UID,
        /// raw 32-byte Solana ed25519 pubkey that owns this account
        owner_pk: vector<u8>,
        funds: Balance<T>,
        /// bump to revoke ALL outstanding session caps at once
        generation: u64,
        /// per-cap cumulative spend ledger keyed by cap ID
        spent: Table<ID, u64>,
    }
}
```

```move
module siws_session::session {
    /// Capability minted to the ephemeral Sui address.
    /// `store` deliberately omitted so it cannot be freely transferred
    /// as an asset; it is address-owned by the temp key.
    public struct SessionCap has key {
        id: UID,
        account_id: ID,
        /// must equal Account.generation or the cap is dead
        generation: u64,
        /// epoch-ms after which cap is invalid
        expires_at_ms: u64,
        /// max cumulative spend over the cap's life
        spend_cap: u64,
        /// per-transaction max
        per_tx_cap: u64,
        /// allowlist of function selectors this cap may call
        allowed: vector<vector<u8>>,
        /// the temp Sui address this was minted for (binding)
        holder: address,
    }
}
```

### 1.3 Registry + nonce store

```move
module siws_session::registry {
    public struct Registry has key {
        id: UID,
        /// solana_pk → Account object ID
        accounts: Table<vector<u8>, ID>,
        /// consumed (pk, nonce) pairs — replay protection
        nonces: Table<vector<u8>, bool>,
    }
}
```

> **Contention note.** `Registry` is written only on *first-ever* sign-in per
> user (insert into `accounts`) and on *every* sign-in (insert nonce). The
> nonce table is the hot path. If global throughput becomes an issue, shard
> the registry into N shared objects keyed by `pk[0] % N`, or move nonce
> tracking into the per-user `Account` (preferred — see 1.5).

**Recommendation:** keep nonce state in the per-user `Account`, not the global
`Registry`. That removes the single global write hotspot. The `Registry` then
only ever does one insert per user, ever.

### 1.4 The message format (must be byte-exact both sides)

Define a canonical message the SDK builds and the contract reconstructs. Do
**not** let the caller pass arbitrary message bytes. The contract rebuilds the
bytes from on-chain/checked values and verifies against that.

```
siws-session-v1
domain: <package_id_hex>
chain: sui:<network>
account: <solana_pk_base58>
session_key: <temp_sui_addr_hex>
generation: <u64>
nonce: <hex_32_bytes>
expires_at_ms: <u64>
```

Encode each field deterministically (fixed key order, single `\n` separators,
no trailing newline). This is your own format, simpler and stricter than the
full SIWS ABNF — but if you want Phantom to render a familiar SIWS screen,
wrap these as SIWS `Resources`/statement fields and reconstruct accordingly.
The rule that matters: **one canonical serializer, used identically on both
sides.**

### 1.5 Verify + open session

```move
public entry fun verify_and_open_session<T>(
    registry: &mut Registry,
    clock: &Clock,
    solana_pk: vector<u8>,        // 32 bytes
    signature: vector<u8>,        // 64 bytes
    session_key: address,         // temp Sui addr
    generation: u64,
    nonce: vector<u8>,
    expires_at_ms: u64,
    spend_cap: u64,
    per_tx_cap: u64,
    allowed: vector<vector<u8>>,
    ctx: &mut TxContext,
) {
    // 1. freshness
    assert!(clock.timestamp_ms() < expires_at_ms, E_EXPIRED);
    assert!(!registry.nonces.contains(nonce), E_NONCE_USED);

    // 2. reconstruct message bytes from THESE args (not caller-supplied blob)
    let msg = build_message(
        object::id_address(registry), // or fixed package domain
        solana_pk, session_key, generation, nonce, expires_at_ms,
    );

    // 3. verify ed25519
    assert!(ed25519::ed25519_verify(&signature, &solana_pk, &msg), E_BAD_SIG);

    // 4. consume nonce
    registry.nonces.add(nonce, true);

    // 5. find or create the user's Account
    let account_id = if (registry.accounts.contains(solana_pk)) {
        *registry.accounts.borrow(solana_pk)
        // generation check happens when the cap is USED, against the
        // live Account.generation — see app entrypoint
    } else {
        let acct = account::new<T>(solana_pk, ctx);
        let id = object::id(&acct);
        registry.accounts.add(solana_pk, id);
        transfer::public_share_object(acct);
        id
    };

    // 6. mint the cap to the temp key
    let cap = SessionCap {
        id: object::new(ctx),
        account_id,
        generation,
        expires_at_ms,
        spend_cap,
        per_tx_cap,
        allowed,
        holder: session_key,
    };
    transfer::transfer(cap, session_key);
}
```

### 1.6 Scoped app entrypoint (the part that enforces limits)

```move
public entry fun withdraw<T>(
    cap: &SessionCap,
    account: &mut Account<T>,
    clock: &Clock,
    amount: u64,
    recipient: address,
    ctx: &mut TxContext,
) {
    // binding: cap was minted for THIS caller
    assert!(tx_context::sender(ctx) == cap.holder, E_WRONG_HOLDER);
    // cap targets THIS account
    assert!(cap.account_id == object::id(account), E_WRONG_ACCOUNT);
    // not revoked
    assert!(cap.generation == account.generation, E_REVOKED);
    // not expired
    assert!(clock.timestamp_ms() < cap.expires_at_ms, E_EXPIRED);
    // per-tx cap
    assert!(amount <= cap.per_tx_cap, E_OVER_PER_TX);
    // cumulative cap
    let prev = if (account.spent.contains(object::id(cap)))
        *account.spent.borrow(object::id(cap)) else 0;
    assert!(prev + amount <= cap.spend_cap, E_OVER_TOTAL);
    // function allowlist
    assert!(is_allowed(&cap.allowed, b"withdraw"), E_NOT_ALLOWED);

    // update ledger + move funds
    upsert_spent(account, object::id(cap), prev + amount);
    let coin = coin::take(&mut account.funds, amount, ctx);
    transfer::public_transfer(coin, recipient);
}
```

### 1.7 Revocation

```move
/// Requires a fresh root (Solana) signature, same verify path as sign-in
/// but bumps generation, instantly killing every outstanding cap.
public entry fun revoke_all<T>(
    registry: &Registry,
    account: &mut Account<T>,
    clock: &Clock,
    solana_pk: vector<u8>,
    signature: vector<u8>,
    nonce: vector<u8>,
    expires_at_ms: u64,
    ctx: &mut TxContext,
) {
    assert!(account.owner_pk == solana_pk, E_NOT_OWNER);
    // verify a "revoke-v1" domain-separated message (own serializer)
    // ... ed25519_verify ...
    account.generation = account.generation + 1;
}
```

### 1.8 Error codes

| Code | Meaning |
|------|---------|
| `E_EXPIRED` | cap or signature past `expires_at_ms` |
| `E_NONCE_USED` | replay attempt |
| `E_BAD_SIG` | ed25519 verification failed |
| `E_WRONG_HOLDER` | caller ≠ cap.holder |
| `E_WRONG_ACCOUNT` | cap.account_id ≠ account |
| `E_REVOKED` | cap.generation ≠ account.generation |
| `E_OVER_PER_TX` | amount > per_tx_cap |
| `E_OVER_TOTAL` | cumulative > spend_cap |
| `E_NOT_ALLOWED` | selector not in allowlist |
| `E_NOT_OWNER` | revoke by non-owner |

---

## 2. Frontend / SDK

### 2.1 Package surface

```
@yourorg/sui-siws-session
  createSession(opts)        → SessionHandle
  SessionHandle.execute(tx)  → result      // auto-signed, sponsored
  SessionHandle.revoke()
  SessionHandle.status()     → { expiresAt, spent, remaining, generation }
  restoreSession()           → SessionHandle | null
```

### 2.2 Ephemeral key generation (mitigating browser theft)

Generate the temp Sui Ed25519 key as a **non-extractable** WebCrypto key when
the target browsers support Ed25519 in WebCrypto; sign by handing bytes to
`crypto.subtle.sign` so the private key never appears in JS memory.

```ts
// Preferred: non-extractable WebCrypto Ed25519
const kp = await crypto.subtle.generateKey(
  { name: "Ed25519" },
  /* extractable */ false,
  ["sign", "verify"]
);
// Wrap into a Sui Signer that delegates signing to subtle.sign and
// exposes the public key via subtle.exportKey("raw", kp.publicKey).
```

Fallback ladder if WebCrypto Ed25519 is unavailable:
1. **Passkey-backed signer** — use Sui's `PasskeyKeypair`. Origin-bound,
   hardware-protected, cannot be exfiltrated or phished cross-site. This is
   the strongest option and may make the "temp key" itself a passkey rather
   than a software key. Strongly consider making this the default.
2. Software `Ed25519Keypair` kept only in memory (never `localStorage`),
   shortest viable expiry, persist only an encrypted handle.

> **Never** put the raw session secret in `localStorage`/`sessionStorage`.
> If you must survive reloads, store only a non-extractable key handle
> (WebCrypto/passkey); software secrets in storage are the classic theft path.

### 2.3 Sign-in flow

```ts
import { getFullnodeUrl, SuiClient } from "@mysten/sui/client";
import { Transaction } from "@mysten/sui/transactions";

async function createSession(opts: {
  solanaWallet: SolanaSignMessage;   // Phantom etc.
  network: "mainnet" | "testnet";
  packageId: string;
  registryId: string;
  spendCap: bigint;
  perTxCap: bigint;
  ttlMs: number;
  allowed: string[];                 // function selectors
  sponsor: SponsorClient;            // your relayer
}): Promise<SessionHandle> {
  // 1. temp key (see 2.2)
  const tempSigner = await makeNonExtractableSuiSigner();
  const sessionKey = tempSigner.toSuiAddress();

  // 2. build the canonical message — MUST match Move build_message byte-for-byte
  const nonce = crypto.getRandomValues(new Uint8Array(32));
  const expiresAtMs = Date.now() + opts.ttlMs;
  const generation = await fetchGeneration(opts, /* may be 0 if new */);
  const message = serializeSiwsSession({
    domain: opts.packageId,
    chain: `sui:${opts.network}`,
    accountPk: await opts.solanaWallet.getPublicKey(),
    sessionKey,
    generation,
    nonce,
    expiresAtMs,
  });

  // 3. user signs ONCE with Solana wallet
  const { signature } = await opts.solanaWallet.signMessage(message);

  // 4. submit verify_and_open_session as a SPONSORED tx
  const tx = new Transaction();
  tx.moveCall({
    target: `${opts.packageId}::session::verify_and_open_session`,
    typeArguments: [/* coin type */],
    arguments: [ /* registry, clock, pk, sig, sessionKey, ... */ ],
  });
  const result = await opts.sponsor.executeSponsored(tx, tempSigner);

  return new SessionHandle(tempSigner, opts, { expiresAtMs, generation });
}
```

### 2.4 Auto-signed app calls

After sign-in, app interactions are signed by the temp key with **no user
prompt**, and gas is paid by the sponsor. Use `SerialTransactionExecutor` so
rapid sequential calls don't hit object-version errors on the shared Account.

```ts
class SessionHandle {
  async execute(build: (tx: Transaction) => void) {
    if (Date.now() > this.expiresAtMs) throw new SessionExpired();
    const tx = new Transaction();
    build(tx);                          // app adds its moveCall(s)
    // sponsor pays; temp key authorizes
    return this.opts.sponsor.executeSponsored(tx, this.tempSigner);
  }
}
```

### 2.5 Sponsored transaction shape

```ts
// Build tx kind only, hand to sponsor for gas, both signatures execute.
const kindBytes = await tx.build({ client, onlyTransactionKind: true });
// sponsor: wraps with gas data, signs gas; client signs the tx with temp key;
// execute_transaction_block receives [userSig, sponsorSig].
```

The sponsor/relayer is a backend you run: it holds gas coins, validates the
requested move call against an allowlist (defense in depth — don't let the
relayer pay for arbitrary calls), and co-signs. It **cannot** move user funds:
fund movement requires the `SessionCap`, which the relayer doesn't hold.

### 2.6 Session lifecycle UX

- **Renew**: silent re-sign with Solana wallet before `expiresAt`; mint a fresh
  cap (new nonce). One wallet prompt, infrequent.
- **Revoke**: `SessionHandle.revoke()` → root-signed `revoke_all` → generation
  bump → every cap on that account dies immediately.
- **Restore on reload**: only if using passkey/WebCrypto non-extractable key;
  re-derive address, check cap still valid (`status()`), else re-run sign-in.

---

## 3. Security model summary

| Threat | Mitigation |
|--------|-----------|
| Signature replay | per-message nonce consumed on-chain; expiry vs `Clock` |
| Cross-contract replay | domain field = package id, reconstructed on-chain |
| Caller-forged message | contract rebuilds bytes from args; never trusts blob |
| Stolen session key | spend_cap + per_tx_cap + expiry + allowlist bound damage |
| Key exfiltration from browser | non-extractable WebCrypto / passkey signer |
| Cross-site use | passkey is origin-bound; bind session to origin |
| Mass compromise / lost device | `revoke_all` bumps generation, kills all caps |
| Malicious relayer | relayer pays gas only; cannot hold SessionCap → can't spend |
| Shared-object contention | nonces in per-user Account; registry write once/user; serial executor client-side |

**Residual risk (be honest with users):** within an unexpired cap's remaining
budget and allowlist, a compromised session key can act. Session keys cap
*magnitude and duration* of loss; they don't eliminate it. Set conservative
defaults (short TTL, low per-tx and total caps) and let dApps opt into more.

---

## 4. Build order (suggested)

1. Move package + Move unit tests for verify/replay/expiry/cap/revoke. Get the
   serializer byte-exact against a TS reference vector first — this is the
   single highest-risk integration point.
2. Sponsor/relayer service (gas pool, call allowlist, co-sign).
3. SDK: temp-key signer (passkey first), sign-in, sponsored execute.
4. React bindings / `dapp-kit` integration, status + revoke UI.
5. Multi-wallet: abstract the "Sign In With" signer so SIWE/other Ed25519 or
   secp256k1 roots plug in (secp256k1 root → use Sui's `ecdsa_k1` verify path
   instead of `ed25519`).

---

## 5. Open design decisions to settle early

- **Coin generality**: single `Coin<T>` per Account, or multi-asset via
  dynamic fields? Multi-asset is more useful but complicates the spend ledger.
- **Allowlist granularity**: function-name bytes (simple) vs full
  package::module::function selectors (safer). Prefer the latter.
- **Passkey vs software temp key as the default** — passkey is materially safer
  and Sui supports it natively; the only cost is the per-session biometric tap,
  which arguably defeats "no signing" for some flows. Consider passkey for
  high-value caps, software key for low-value.
- **Whether you even need the custom Account** vs leaning on Sui's native
  zkLogin/passkey + sponsored tx for same-chain users, reserving this whole
  system for the genuinely cross-chain (Solana-identity) case where it's
  actually differentiated.
