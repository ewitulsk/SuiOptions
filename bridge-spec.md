# Cross-Chain Messaging Layer + Lock-and-Mint Bridge — Implementation Spec

**Version:** 0.2 (architecture draft — revised after design review; Nautilus/Seal mechanics verified against MystenLabs docs and `seal_policy.move`)
**Scope of v1:** HyperEVM testnet ⇄ Sui testnet, single enclave (1-of-1, architected for k-of-n), Mysten open Seal testnet key servers.
**Status:** Specification only. No production code in this document.

---

## 0. Reading guide

Two facts define the roles of the cryptographic components in this design:

- **Seal encrypts; it does not sign.** Seal's role is to encrypt each signer's key share at rest so that share can be decrypted only inside an attested Nautilus enclave running approved code. The signer is the **Nautilus TEE(s)** running a threshold-signature protocol.
- **The signing key is produced by Distributed Key Generation (DKG).** Participants run a multi-round protocol, each ends holding a *verified share*, the group public key falls out, and the full private key never exists anywhere — including inside any single TEE.

The system has **three layers**, strictly separated:

- **Layer 1 — Generic cross-chain messaging.** Seal+Nautilus threshold signers are the transport. Carries arbitrary payloads addressed to destination apps.
- **Layer 2 — Bridge app (Locker).** An NTT/OFT-style, one-deployment-per-asset lock-and-mint application built *on top of* Layer 1. Knows nothing about enclaves or signatures.
- **Layer 3 — Clients & relayers.** Fully untrusted plumbing that ferries self-verifying signed messages on-chain.

---

## 1. Trust model (read this before any component)

| Property | Guarantee | Depends on |
|---|---|---|
| **Message authenticity** | A destination Inbox accepts a message only if it carries a valid aggregated threshold signature from the registered signer group. | Threshold crypto (k-of-n), NOT the TEE. |
| **Share confidentiality** | No party (operator, host, attacker) can read a signer's key share at rest or in use. | Seal policy (PCR-gated) + Nautilus enclave memory isolation. |
| **Honest-code attestation** | A signer only participates if peers verify its Nautilus attestation (PCRs) and on-chain-registered pubkey. | Nautilus remote attestation + on-chain Enclave registry. |
| **No key reconstruction** | The full private key is never assembled anywhere, including at signing time. | FROST / GG20 threshold signing. |
| **Liveness** | Bridge progresses as long as ≥ k signers are online and ≥1 relayer is willing. | k-of-n availability; permissionless relay. |
| **Replay safety** | Each message executes at most once on the destination, on this deployment only. | Consumed-hash set on Inbox + per-deployment domain separator in the digest (§2.2, §2.6). |
| **Source truth** | A release/mint is signed only for messages a canonical Outbox committed at source finality. | Outbox commitment + per-chain finality gate. |
| **Chain-view integrity** | A signer's view of "committed at finality" cannot be forged by its own untrusted host. | TLS terminated inside the enclave + ≥2 independent RPC providers (§5.4). At N ≥ 3 the threshold also absorbs single-host MITM. |
| **Per-node share isolation** | A key share is decryptable only by the specific registered node instance — never by any enclave that merely runs the same code. | Per-node Seal policy binding (§6.5). PCR-only policies forbidden. |
| **Key-server honesty** | Share confidentiality at rest holds only while fewer than t of the chosen Seal key servers are compromised. | t-of-n Seal key-server selection (§6.6). |

**TEE role.** Under threshold signing with no reconstruction, the TEE is **defense-in-depth**, not the trust root. Even a fully compromised single enclave cannot forge a group signature unless the attacker also controls ≥ k shares. At the **N=1 launch** this distinction collapses (1-of-1 ≡ a single signer ≡ trust the one TEE+Seal); the strong guarantee arrives only at **N ≥ 3, k = 2**. State this plainly to stakeholders: *launch security = single TEE + its host's network path; threshold security = a later phase turn-on.* The system-wide security ceiling is **min(k honest signer operators, t honest Seal key-server operators, integrity of each signer's chain view)** — the cryptography makes these thresholds enforceable; only operational independence makes them real.

---

## 2. Layer 1 — Generic Cross-Chain Messaging

### 2.1 Components

```
Source chain                         Off-chain                    Destination chain
┌──────────────┐   commit at        ┌──────────────────┐  deliver ┌──────────────┐
│   Outbox     │─── finality ──────▶│  Signer group     │────────▶│   Inbox      │
│ (per chain)  │   (event/root)     │  (N Nautilus TEEs │ (relay) │ (per chain)  │
└──────────────┘                    │   + Seal shares)  │         └──────────────┘
       ▲                            └──────────────────┘                │
       │ send(dst, app, payload)                                        │ receive()
┌──────────────┐                                                ┌──────────────┐
│ App (Locker) │                                                │ App (Locker) │
└──────────────┘                                                └──────────────┘
```

- **Outbox** (one per chain): apps call it to emit a message; it assigns a nonce, computes the canonical message hash, and commits it (event + optional accumulator root) so the signer group can observe it deterministically.
- **Signer group** (N Nautilus enclaves): each holds a Seal-encrypted share; on request, independently verifies that a registered Outbox committed the message at source finality (§5.3–§5.4); runs threshold signing; emits one aggregated signature per message.
- **Inbox** (one per chain): verifies the aggregated signature against the registered group public key for the message's signature scheme, enforces nonce + hash dedup, and dispatches the payload to the destination app.

### 2.2 Canonical message format (chain-neutral)

The signers sign over a **canonical serialization** that is identical regardless of source/destination encoding. Per-chain adapters encode/decode but never change semantics.

```
CrossChainMessage {
  version:        u8                 // format version, start at 1
  src_chain_id:   u32                // internal registry ID (NOT the native chainid)
  dst_chain_id:   u32                // internal registry ID
  nonce:          u64                // uniqueness salt, assigned monotonically per (src_chain_id, dst_chain_id); no ordering semantics (§2.6)
  src_app:        bytes32            // sender app address, left-padded
  dst_app:        bytes32            // recipient app address, left-padded
  payload:        bytes              // opaque to Layer 1; app-defined
}

DOMAIN_SEP   = keccak256("XCHAIN_MSG_V1" || deployment_salt)   // deployment_salt fixed at genesis, unique per deployment
message_hash = keccak256(DOMAIN_SEP || canonical_bcs_or_abi(CrossChainMessage))
```

Design notes:
- **Internal chain IDs**, not native chain IDs, so chains without an EVM-style chainid (Sui) fit uniformly. A registry maps `internal_id ⇄ {native identifier, Outbox addr, Inbox addr, finality params}`.
- `bytes32` addresses on both sides; Sui object/package IDs are 32 bytes, EVM addresses are left-padded to 32. Generic for HyperEVM+Sui today, room for others.
- The **hash function is fixed to keccak256** for both chains (cheap on EVM; available in Move) so the signed digest is identical everywhere.
- **Domain separation is mandatory.** The digest binds a per-deployment salt. Without it, redeploying an Inbox (fresh `consumed` set, same registry IDs — which *will* happen on testnet) would let every previously signed message replay against the new deployment; it also rules out any cross-context reuse of group-key signatures.
- `payload` is fully opaque to Layer 1 — this is what makes it a *generic* messaging layer rather than a bridge-specific one.

### 2.3 Signature schemes (dual, scheme-tagged)

The delivered message carries a `(scheme_tag, group_pubkey_id, aggregated_signature)` envelope. The Inbox selects the verifier by `scheme_tag`:

| Destination family | Scheme | On-chain verification | Group key |
|---|---|---|---|
| EVM (HyperEVM) | **GG20 / CGGMP threshold ECDSA (secp256k1)** | `ecrecover` → compare to registered group address | secp256k1 group key |
| Sui (and other Ed25519 chains) | **FROST threshold Schnorr (Ed25519)** | Ed25519 verify against registered group pubkey | Ed25519 group key |

This requires **two DKGs** (one per curve) producing two group keys, both share-held, both Seal-gated. The envelope's `group_pubkey_id` lets the Inbox look up which registered key to check, enabling key rotation without ABI changes.

### 2.4 Outbox interface (abstract; both chains implement)

```
send(dst_chain_id: u32, dst_app: bytes32, payload: bytes) -> (nonce: u64, message_hash: bytes32)
  - caller = src_app (recorded)
  - assigns nonce = next_nonce[dst_chain_id]++
  - emits MessageCommitted(message_hash, full CrossChainMessage fields)
  - reverts if Outbox is paused

view: next_nonce(dst_chain_id) -> u64
view: is_committed(message_hash) -> bool
admin: setPaused(bool)            // guardian only — global circuit breaker for this chain's outbound
```

**Per-family caller identity (`src_app`):**
- **EVM:** `src_app = msg.sender`, left-padded to 32 bytes.
- **Sui:** packages have no `msg.sender`. The Outbox issues an **`EmitterCap`** object per app at registration; `send` takes `&EmitterCap` and records `src_app = the cap's ID`. (Wormhole-on-Sui emitter pattern.)

### 2.5 Inbox interface (abstract; both chains implement)

```
receive(message: CrossChainMessage, envelope: SignatureEnvelope)
  1. require !paused
  2. recompute message_hash from message (with DOMAIN_SEP)
  3. require message.dst_chain_id == THIS_CHAIN_ID
  4. require !consumed[message_hash]              // hash dedup — the sole exactly-once guard (§2.6)
  5. verify envelope against registered group key for envelope.scheme_tag
  6. consumed[message_hash] = true
  7. deliver payload to message.dst_app (per-family, below)
  8. emit MessageDelivered(message_hash)
  - relayer is untrusted: all checks are self-contained on-chain

admin: setPaused(bool)                    // guardian only
admin: registerGroupKey(scheme_tag, key)  // governance — supports rotation
admin: setSignerThreshold(k, n)           // governance
```

**Per-family delivery (step 7):**
- **EVM:** the Inbox calls `IMessageRecipient(dst_app).onReceive(src_chain_id, src_app, payload)` directly — dynamic dispatch exists.
- **Sui:** Move has **no dynamic dispatch**; the Inbox cannot call an arbitrary `dst_app`. Delivery inverts: `receive` verifies and returns a **hot-potato receipt** `DeliveredMessage { src_chain_id, src_app, dst_app, payload }` (no `drop`/`store` abilities). The relayer's PTB must, in the same transaction, pass it to the destination app's own entry function (e.g. `locker::consume(receipt, …)`), which asserts `receipt.dst_app == its own registered identity` before effecting. The hot potato makes verification and consumption atomic — the receipt cannot be stored, dropped, or smuggled out. (This is the Wormhole-on-Sui VAA pattern.)

### 2.6 Dedup policy (no ordering — resolved)

- **The Inbox enforces no cross-message ordering.** The nonce exists only to make otherwise-identical messages (same route, apps, payload) hash-distinct; the Outbox assigns it monotonically per `(src_chain_id, dst_chain_id)`, but the Inbox never checks sequence.
- **Hash dedup**: `consumed[message_hash]` is the sole exactly-once guard.
- Rationale: transfers are independent, so ordering buys nothing. Strict ordering turns the permissionless Outbox into a DoS lever (anyone can `send()` garbage that must then be delivered in order before real transfers land), and one undeliverable message wedges the whole lane. Dedup-only is the Wormhole model and is strictly simpler than a windowed seen-set. Apps that ever need ordering can sequence in their own payloads.

### 2.7 Delivery-failure semantics

- **Failure is atomic and retryable.** `consumed[message_hash]` is set in the same transaction as the app effect on both chains (EVM: a reverting `onReceive` reverts the whole `receive`; Sui: an aborting `consume` aborts the whole PTB, hot potato included). A failed delivery therefore leaves the message unconsumed and indefinitely retryable — no message is ever half-delivered.
- **A stuck message harms only itself.** Because the Inbox enforces no ordering (§2.6), an undeliverable message (malformed payload, unregistered peer, paused Locker) blocks nothing else on the lane.
- **v1 accepts permanently stuck messages.** There is no Layer 1 refund/recovery path: the only v1 sender is the Locker, which constructs payloads by code, so a permanently undeliverable message implies a bug — handled by governance (peer re-registration, contract upgrade), not protocol machinery. If richer recovery is ever needed, a "verify-and-store, execute separately" split (Wormhole-style) can be added without changing the message format.

### 2.8 Pausing (Layer 1 circuit breaker)

Both Outbox and Inbox are independently pausable by a **guardian** (multisig/governance). Pausing the Inbox halts *all inbound delivery* on that chain; pausing the Outbox halts *all outbound*. This is the global kill switch, distinct from per-asset Locker pausing (§3.5).

---

## 3. Layer 2 — Lock-and-Mint Bridge (NTT/OFT-style)

### 3.1 Model

One Locker deployment **per asset per chain**, mirroring Wormhole NTT "manager" + transceiver and LayerZero OFT. Layer 1 plays the **transceiver** role; the Locker is the **manager**.

- **Home chain** (where the asset is native): Locker is a **lock/escrow vault**.
- **Foreign chain**: Locker controls a **wrapped asset** with mint/burn authority.
  - Sui: wrapped `Coin<T>` whose `TreasuryCap` is held inside the shared Locker object (packages cannot own objects on Sui). Each asset's wrapped coin needs its own one-time-witness package, so onboarding an asset on Sui = publish a small coin package + hand its cap to a new Locker — a publish, not a config call. Factor this into the asset-onboarding runbook.
  - HyperEVM: wrapped ERC-20 where the Locker holds mint/burn rights.
- **Invariant** (per asset per route): `wrapped_supply_on_foreign ≤ locked_collateral_on_home`. Enforced by construction: foreign mint happens only on a delivered burn-or-lock message; home release only on a delivered burn message.

### 3.2 Bridge-to-Sui flow (HyperEVM → Sui), lock-and-mint

```
1. User calls Locker(HyperEVM).lock(amount, sui_recipient)
2. Locker escrows `amount`, builds payload = LockMsg{asset_id, amount, recipient=sui_recipient}
3. Locker calls Outbox(HyperEVM).send(dst=Sui, dst_app=Locker(Sui), payload)
4. Outbox commits message at HyperEVM finality (confirmation depth, §4)
5. Signer group observes committed message → threshold-signs with FROST-Ed25519 (dst=Sui)
6. Any relayer submits one PTB calling Inbox(Sui).receive(message, envelope)
7. Inbox(Sui) verifies Ed25519 group sig + hash dedup, returns hot-potato DeliveredMessage (§2.5)
8. Same PTB: Locker(Sui).consume(receipt) asserts dst_app == self, mints wrapped Coin<T>
   to sui_recipient (or queues the transfer if the rate limit is exceeded, §3.5)
```

### 3.3 Bridge-from-Sui flow (Sui → HyperEVM), burn-to-release

```
1. User calls Locker(Sui).burn(wrapped_coin, evm_recipient)
2. Locker(Sui) burns wrapped via TreasuryCap, builds payload = BurnMsg{asset_id, amount, recipient=evm_recipient}
3. Locker(Sui) calls Outbox(Sui).send(dst=HyperEVM, dst_app=Locker(HyperEVM), payload)
4. Outbox commits at Sui checkpoint finality
5. Signer group threshold-signs with GG20 ECDSA (dst=HyperEVM, ecrecover-compatible)
6. Any relayer calls Inbox(HyperEVM).receive(message, envelope)
7. Inbox(HyperEVM) ecrecovers group address + hash dedup, calls Locker(HyperEVM).onReceive
8. Locker(HyperEVM).onReceive(payload) releases escrowed native to evm_recipient
   (or queues the transfer if the rate limit is exceeded, §3.5)
```

### 3.4 Locker (app) interface

```
// outbound
lock(amount, dst_recipient: bytes32)     // home chain
burn(amount, dst_recipient: bytes32)     // foreign chain

// inbound
onReceive(src_chain_id, src_app, payload)   // EVM: called only by the local Inbox
consume(receipt: DeliveredMessage, ...)     // Sui: consumes the Inbox hot potato (§2.5)
  - require caller == Inbox (EVM) / receipt originates from the local Inbox (Sui)
  - require src_app == registered peer Locker for this asset
  - decode {asset_id, amount, recipient}
  - if within rate limit: home → release escrow to recipient; foreign → mint wrapped to recipient
  - else: enqueue {recipient, amount, unlock_at} — never revert (§3.5)

claim(queued_transfer_id)                   // permissionless: releases a queued transfer
                                            // once its unlock time has passed

// admin
setPaused(bool)                 // per-asset guardian
setPeer(chain_id, locker_addr)  // governance: trusted sibling Locker
setRateLimit(window, cap)       // governance (recommended default ON)
```

### 3.5 Emergency controls (two independent levels)

- **Per-asset halt**: pause one Locker → stops that asset only. Free from the one-deployment-per-asset model.
- **Layer 1 global halt**: pause Outbox/Inbox → stops *all* messaging on a chain.
- **Per-asset rate limits (default ON — resolved).** NTT treats outbound+inbound rate-limiting as core. A capped-per-window limit is the cheapest insurance that a signer-key compromise can't drain everything in one transaction.
- **Rate-limit overflow queues; it never reverts.** By the time the Locker runs, the message is consumed at the Inbox (or the whole tx reverts and the lane retries forever). Reverting on an exceeded limit would strand the user's funds at source with no recovery path until the window resets. Instead, NTT-style: record the transfer in an on-chain queue with an unlock time; `claim` is permissionless after the window. Delivery always succeeds; only the payout is delayed.

---

## 4. Finality handling (per chain)

The enclave signs **only after source finality**:

- **HyperEVM**: configurable **confirmation depth**. ⚠️ **Open item to verify at build time:** HyperEVM is the EVM execution layer of Hyperliquid (HyperBFT/HyperCore consensus), not a generic PoW/PoS EVM — its finality semantics and reorg behavior must be confirmed against current Hyperliquid docs before fixing the confirmation parameter. Do not assume Ethereum-style finality. Also confirm the **dual-block architecture** (frequent small blocks vs ~once-a-minute big blocks with separate gas limits): it affects both what "confirmation depth" means and which block type Inbox/Locker transactions land in.
- **Sui**: wait until the source transaction is in a **finalized checkpoint** (Sui has fast deterministic finality; effectively no reorgs once checkpointed).
- No optimistic path in v1 (keeps the trust model clean).

The finality parameters live in the chain registry (§2.2) so they are tunable per chain without code changes.

---

## 5. The signer node (Nautilus enclave application)

### 5.1 Base

Fork **`MystenLabs/nautilus`**, app at `src/nautilus-server/src/apps/seal-example`. That example already implements: PCR-gated Seal key-load (2-step host-delegated fetch), in-enclave key caching, an Ed25519 ephemeral key registered on-chain in an `Enclave` object, and signed response envelopes. We replace the "weather API key" provisioning with **threshold-signing-share** provisioning, replace `/process_data` with bridge-message signing, and replace the example Seal policy outright — the stock `seal_policy.move` authorizes *any* registered enclave with matching PCRs, which is unsafe for key shares (§6.5).

### 5.2 Keys held inside the enclave (in memory only)

| Key | Type | Purpose |
|---|---|---|
| Ephemeral key | Ed25519 | Signs the Seal `seal_approve` PTB intent + authenticates to peers; registered on-chain (per Nautilus pattern). |
| Seal wallet | Ed25519 | Seal certificate signing + tx sender for `seal_approve`. |
| ElGamal enc key | BLS group elems | Decrypts Seal key-load responses inside the enclave. |
| **ECDSA share** | secp256k1 (GG20/CGGMP) | This node's share of the EVM-destined group key. **Seal-encrypted at rest.** |
| **Ed25519 share** | Ed25519 (FROST) | This node's share of the Sui-destined group key. **Seal-encrypted at rest.** |

Only the **shares** are Seal-stored. The group private keys are never stored or reconstructed.

### 5.3 Endpoints

**Public (port 3000):**
```
GET  /get_attestation                 // Nautilus attestation doc (PCRs, eph pubkey)
POST /sign_requests                   // {message: CrossChainMessage} → 202 + request accepted
                                       //   idempotent per message_hash; cheap pre-check that the
                                       //   hash is committed on a registered Outbox before queueing
GET  /sign_requests/{message_hash}    // pending | signed {envelope} | rejected {reason}
GET  /health
```

Signing is **request-triggered and asynchronous**: FROST/GG20 at k > 1 are multi-round protocols across nodes, so a synchronous request/response API cannot survive M3 — design the poll model now so the interface doesn't break when MPC turns on. One signing session per `message_hash`, never per request. The endpoint is public and therefore a DoS surface: reject anything not already committed on a registered Outbox *before* doing expensive work, dedupe in-flight hashes, and rate-limit per source.

**Peer-to-peer (MPC mesh, authenticated):**
```
MPC round transport (DKG + signing rounds) — libp2p, see §6.3
```

**Admin (port 3001, localhost on the EC2 host only):**
```
POST /admin/init_seal_key_load        // returns encoded FetchKeyRequest (per Nautilus-Seal)
POST /admin/complete_seal_key_load    // caches decrypted Seal keys in enclave memory
POST /admin/provision_ecdsa_share     // load Seal-encrypted secp256k1 share
POST /admin/provision_ed25519_share   // load Seal-encrypted Ed25519 share
POST /admin/dkg/start                 // begin a DKG round (ceremony, §6)
```

### 5.4 What the enclave checks before signing (the security boundary)

For each `/sign_message`:
1. Verify the message was **committed by the registered Outbox** on the named source chain (read via the enclave's own trusted full-node/RPC view).
2. Verify **source finality** per §4.
3. Verify the message is **well-formed** and `dst_chain_id` is a registered route.
4. Only then enter the threshold-signing round.

This is the narrow, auditable check: "did the canonical Outbox commit this exact message at finality," not free-form event scraping.

**The chain view is part of the security boundary.** Nautilus enclaves have no direct network: all traffic is forwarded by the untrusted parent EC2 host (vsock + `allowed_endpoints.yaml`). A host that can MITM the enclave's RPC reads can fabricate "committed at finality" and get an arbitrary message signed — no enclave compromise needed. Therefore:

1. **TLS terminates inside the enclave**, pinned to named RPC providers; the host forwards opaque bytes only.
2. Commitment + finality are confirmed against **≥ 2 independent RPC providers** before signing.
3. At N = 1 the host's network path is squarely in the TCB (stated in the §1 table); at N ≥ 3 an attacker must MITM k independent operators' hosts, so the threshold absorbs a single bad host.

---

## 6. The DKG ceremony & threshold signing

### 6.1 Primitive choice (confirmed)

- **Generation:** Distributed Key Generation (Pedersen/GJKR-style; FROST has its own DKG, "DKG for FROST"/`trusted-dealer`-free variant).
- **Signing:** **FROST** (2-round) for Ed25519; **GG20/CGGMP** (multi-round) for secp256k1 ECDSA.
- **Build-vs-buy: reuse audited libraries. Do NOT roll your own threshold crypto.**
  - FROST: a maintained `frost-ed25519` crate (ZF FROST family is the well-trodden choice).
  - ECDSA: a maintained GG20/CGGMP implementation.
  - ⚠️ **Open item:** the ECDSA-MPC library landscape varies in quality/maintenance and shifts over time. The specific crate must get a security review and a current-maintenance check at build time rather than being hard-committed from memory now. Selection criteria beyond maintenance: **identifiable aborts** (a misbehaving party can be pinpointed and ejected rather than silently stalling rounds) and **safe concurrent signing sessions** (multiple `message_hash` sessions in flight at once, per §5.3).

### 6.2 Participants

DKG parties = a mix of **IRL human participants** and **one or more Nautilus TEE participants**, all as equal DKG parties. Each party finishes holding a *verified share*; no party ever holds the whole key, and there is no central aggregation step. After DKG:
- Each TEE party's share is immediately **Seal-encrypted to that node's identity** under the per-node policy of §6.5. **PCR-only policies are forbidden:** all signer enclaves run identical code and therefore have identical PCRs, so a PCR-gated share could be decrypted by *any* operator's legitimately attested enclave — one malicious operator could collect k ciphertexts and reconstruct, collapsing k-of-n to 1.
- Human-held shares (if any persist beyond bootstrapping) need their own custody story — **decide whether humans are bootstrap-only or permanent share-holders** (open item §9).

Run the DKG **twice** — once for the secp256k1 group key, once for the Ed25519 group key.

### 6.3 MPC transport (standard node approach)

- **libp2p P2P mesh** between enclaves over **mutually-attested, authenticated channels**: each node verifies peers' Nautilus attestation + on-chain-registered pubkey before accepting round messages.
- An **untrusted coordinator/relayer for liveness only**: queues/forwards round messages; cannot forge them (every round message is signed by a share-holder's enclave key). Can stall, never corrupt.
- At **N=1 launch** there is no mesh (single party); the transport turns on when N grows — no contract changes required.

### 6.4 Lifecycle: restart, recovery, rotation

- **Restart:** enclave loses in-memory shares → re-run the Seal 2-step key-load → re-decrypt its share from Seal → resume. No new DKG. Persistence is at the *share* level, which preserves the no-reconstruction property.
- **Node recovery / replacement:** provision a fresh enclave with identical PCRs; the node's **operator re-registers** the new instance's attested ephemeral pubkey into that node's `Enclave` object (an explicit, on-chain-visible authorization via the operator's cap — anomalous re-registrations are alertable); it then reloads the same Seal-encrypted share via §6.5. Identical PCRs alone are deliberately **not** sufficient.
- **Membership rotation / proactive refresh:** changing the party set or refreshing shares requires a **re-share / re-run DKG** (group key can stay fixed via resharing, or rotate via fresh DKG + `registerGroupKey` on each Inbox). Supported by the rotation-friendly `group_pubkey_id` in the envelope.

### 6.5 Seal share policy (per-node binding — normative)

Verified against the Nautilus `seal_policy.move` example and the Seal design docs. The stock example policy checks (a) a fixed identity `vector[0]`, (b) tx sender == wallet pk, and (c) an Ed25519 intent signature against `enclave.pk()` — where `enclave` is **whichever registered `Enclave<T>` object the caller passes in**. It therefore authorizes *any* attested instance under the config: correct for one app-wide secret, unsafe for per-node key shares.

Our policy, per share:

- **Identity** = `[node_enclave_object_id]` — one identity per node, not a shared `0x00`.
- `seal_approve(id, signature, wallet_pk, timestamp, enclave: &Enclave<BRIDGE_SIGNER>, ctx)` keeps the example's three checks **and additionally asserts `object::id(enclave) == id`**, binding the share to exactly one node's registry entry.
- The node's `Enclave` object is updateable only via that **operator's cap**, so the cap is the per-node credential. Its custody (hardware wallet vs per-operator multisig) is an open item (§9).
- The **policy package must be immutable** (or upgrade-governed). Seal docs: "if a package is upgradeable, the access control policy can be changed at any time by the package owner."
- The **key-server set is frozen per ciphertext** ("The set of key servers is not dynamic once the data is encrypted"): rotating Seal servers means re-encrypting each share to the new set — cheap, since shares are tiny; this is Seal's own envelope-encryption recommendation applied to shares.

Result, by construction: one malicious operator gets exactly **one** share. The threshold is defeated only by k colluding operators — or by whoever controls ≥ k operator caps, which is why cap custody and genuine operator independence matter more than any of this Move code.

### 6.6 Seal key-server trust layer

Seal privacy is t-of-n over the chosen key servers: a colluding quorum of t can derive the key for any identity in our namespace — i.e. decrypt **every node's share ciphertext**. This layer sits *above* the bridge's k-of-n and is stated in the §1 table.

- **v1 testnet:** Mysten's open testnet key servers. Acceptable for testnet only.
- **Mainnet:** vetted independent operators at t ≥ 2 (Seal security best practices: "treat key server selection as a trust decision"; establish availability agreements), or a committee-mode (MPC) key server. Decide before mainnet (§9).

---

## 7. Chain registry (generic seam, two chains implemented)

A small on-chain + enclave-side registry, designed generic, populated with two entries for v1:

```
ChainRegistry[internal_id] = {
  native_identifier:  bytes        // EVM chainId (HyperEVM) or Sui chain identifier
  family:             enum         // EVM | SUI  (selects sig scheme + adapter)
  outbox_addr:        bytes32
  inbox_addr:         bytes32
  finality:           { kind, depth_or_checkpoint_rule }
}
```

This is where "keep the abstraction generic, implement only HyperEVM+Sui" lives: the registry, the `family` enum, and the per-family encode/verify adapters are the only places a third chain would later plug in.

---

## 8. Milestones (testnet-first, single enclave)

**M0 — Repos & skeletons.** Fork `nautilus`; stand up Move packages (`enclave`, messaging, locker) and Solidity packages (messaging, locker) as interface stubs. Chain registry with HyperEVM-testnet + Sui-testnet entries.

**M1 — Layer 1 messaging, 1-of-1.** Outbox/Inbox on both chains — the Sui Inbox uses the hot-potato receipt pattern from day one (§2.5), and the digest includes `DOMAIN_SEP` from day one (§2.2). Single enclave signs (no MPC yet): GG20 path stubbed to a single-party ECDSA, FROST path stubbed to single-party Ed25519. In-enclave TLS with the dual-provider commitment check (§5.4). Seal key-load working end-to-end (the Nautilus-Seal 2-step) on Mysten open testnet servers, using the per-node policy (§6.5) even at N=1. Permissionless relayer script. **Exit:** a signed generic message delivers end-to-end both directions, self-verifying on-chain.

**M2 — Locker app (lock-and-mint).** Per-asset Locker on both chains (escrow on home, wrapped Coin<T>/ERC-20 on foreign). Hash dedup, peer registration, per-asset pause, rate-limit with overflow queue + permissionless `claim` (§3.5). **Exit:** round-trip a test asset HyperEVM→Sui→HyperEVM with supply invariant holding, including a rate-limited transfer that queues and later claims.

**M3 — Real threshold crypto.** Integrate audited FROST + GG20 libraries. Stand up DKG ceremony tooling. Move to N≥3, k=2 on testnet. libp2p attested mesh + liveness coordinator. **Exit:** group key generated by DKG (never reconstructed), k-of-n signing live, one-node-down tolerated.

**M4 — Lifecycle & hardening.** Restart/recovery from Seal (including the operator re-registration step, §6.4), membership rotation + `registerGroupKey`, guardian/governance wiring for all pause/threshold/peer setters, finality-parameter confirmation for HyperEVM. Security review of MPC library choice **and of our Nautilus fork** — the upstream template is explicitly unaudited ("for evaluation purposes only"). **Exit:** runbook-complete, audit-ready.

---

## 9. Open items to resolve before/within build (not blocking the architecture)

1. **HyperEVM finality semantics + dual-block architecture** — confirm against current Hyperliquid docs; set confirmation depth accordingly (§4).
2. **ECDSA-MPC library selection** — current-maintenance + security review at build time; prefer implementations with identifiable aborts and safe concurrent-session handling (§6.1).
3. **Human DKG participants: bootstrap-only or permanent share-holders?** Affects custody design for non-TEE shares (§6.2).
4. **Operator-cap custody topology** — hardware wallet vs per-operator multisig for the `Enclave`-object registration caps (§6.5).
5. **Mainnet Seal key-server set** — vetted independent t-of-n or committee mode; testnet uses Mysten open servers (§6.6).
6. **Relayer economics** — permissionless relayers pay destination gas with no fee mechanism specified. v1 recommendation: self-relay (our frontend/relayer eats the gas); decide whether a fee mechanism is ever needed.
7. **Wrapped-asset metadata/decimals normalization** across HyperEVM ERC-20 ↔ Sui Coin<T> (NTT-style trimmed-amount handling) — flag for the Locker decode path (§3).
8. **Governance/guardian key** — who holds pause + registry authority; multisig topology.

Resolved since v0.1: nonce policy (no ordering — dedup only, §2.6); rate-limit default (ON, overflow queues, §3.5); Seal policy shape (per-node binding, §6.5); signing API shape (async, request-triggered, §5.3).

---

## 10. What this design deliberately does NOT do

- No Wormhole / LayerZero dependency (removed by design; our Seal+Nautilus signers *are* the transport).
- No key reconstruction at signing time.
- No trust in relayers (messages self-verify on-chain).
- No optimistic delivery in v1 (finality-gated only).
- No single-key-in-Seal (shares only).
- No PCR-only Seal policies — every share is bound to one node's registered on-chain identity (§6.5).
- No ordering guarantees at Layer 1 (dedup-only delivery; apps needing order sequence it themselves).
- No dynamic-dispatch assumptions on Sui (hot-potato receipt, not callbacks, §2.5).
- No revert-on-rate-limit (overflow queues with delayed claim, §3.5).
