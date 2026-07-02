# sui-bridge-contracts/sui

Sui (Move) side of **Layer 1 — Generic Cross-Chain Messaging** from
[`../../bridge-spec.md`](../../bridge-spec.md). This is the on-chain transport: an
Outbox that commits canonical messages, and an Inbox that verifies an
aggregated threshold signature and delivers the payload to a destination app.
It knows nothing about enclaves, DKG, or assets — those live elsewhere.

```
cd sui-bridge-contracts/sui && sui move test && sui move build
```

## Modules

| Module | Responsibility |
|--------|----------------|
| [`chain_id.move`](sources/chain_id.move) | Self-describing internal chain id: `(family << 27) \| local`, family = top 5 bits (1=Sui, 2=EVM, 3=Solana, 4=Aptos) |
| [`message.move`](sources/message.move) | `CrossChainMessage` + the fixed big-endian packed canonical encoding + `keccak256` digest (spec §2.2) |
| [`envelope.move`](sources/envelope.move) | `SignatureEnvelope` (`scheme_tag`, `group_pubkey_id`, `signature`) + Ed25519 verify adapter (spec §2.3) |
| [`registry.move`](sources/registry.move) | `ChainRegistry`, `GroupKeyRegistry`, `GuardianCap`/`GovernanceCap` (spec §7) |
| [`outbox.move`](sources/outbox.move) | `send` → nonce + `MessageCommitted` event, pausable (spec §2.4) |
| [`inbox.move`](sources/inbox.move) | `receive`/`consume` → verify + exactly-once + hot-potato dispatch, pausable (spec §2.5) |
| `events.move` · `errors.move` | Event types and abort codes |

## Key design decisions

- **Canonical wire format is an explicit big-endian packed layout, not BCS.**
  The digest must be byte-identical on Sui and EVM so one signature verifies on
  both. BCS (little-endian, ULEB128 lengths) can't be reproduced by an EVM
  `abi.encodePacked`; the fixed layout in `message.move` can. The
  `known_digest_vector` test pins this against an independent off-chain keccak.

- **Signers sign the 32-byte `message::hash` digest.** Ed25519 verify runs over
  the digest bytes — the same preimage the EVM `ecrecover` path will use.

- **Dispatch is a two-step hot-potato handshake** (`receive` → `consume`), since
  Move has no dynamic dispatch. `receive` verifies and returns an
  ability-less `DeliveredMessage`; the destination app discharges it via
  `consume(..., app: &UID)`, proving identity because only the app's own module
  can produce a `&UID` whose id equals `dst_app`. Replay-marking happens inside
  `consume`, atomically with delivery, so an untrusted relayer cannot
  consume-without-delivering.

- **No cross-message ordering** (spec §2.6): the `consumed` hash-set is the sole
  exactly-once guard; per-source nonce is tracked for observability, not enforced.

## Out of scope here (per the spec's milestones)

- **EVM side** (Solidity Outbox/Inbox) — the `ecrecover`/GG20 verify path.
- **Layer 2 Locker** (lock-and-mint app, wrapped `Coin<T>`) — consumes this
  Inbox's `DeliveredMessage` and calls this Outbox's `send`.
- **Signer node / DKG / Seal** — the off-chain Nautilus enclave (→ `rust-backend/`)
  and the threshold-crypto ceremony.

The Ed25519 verify path uses a single registered group key, which is exactly
the **M1 "1-of-1"** posture: a single-party aggregated signature is
indistinguishable on-chain from a k-of-n one, so no contract change is needed
when real threshold signing turns on (spec §1, §6.3).
