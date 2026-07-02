# bridge-relayer

The untrusted relayer (bridge-spec.md §2, Layer 3). Watches a source Outbox for
committed messages, reconstructs the canonical message, asks the
[signer node](../bridge-signer-service) for an aggregated signature, and submits
the self-verifying message to the destination Inbox. The relayer is trustless —
every check is on-chain.

```
cargo run -p bridge-relayer -- --config config.toml
```

## Pipeline

```
Sui Outbox  ──poll MessageCommitted──▶ reconstruct CrossChainMessage (+ verify hash)
     │                                              │
     │                                  POST /sign_message ──▶ signer node
     │                                              │
     └────────────────────────── submit(message, envelope) ──▶ destination Inbox
```

## M1 status

| Stage | Status |
|-------|--------|
| Sui source watcher (`sui_source.rs`) | ✅ real — pages `MessageCommitted` via sui-sdk, reconstructs + hash-checks each message |
| Signer hop (`signer_client.rs`) | ✅ real — HTTP `POST /sign_message` |
| Relay orchestration (`relay.rs`) | ✅ real + tested — sign, submit, dedup against already-delivered |
| **EVM destination submit** (`evm_submit.rs`) | ✅ real — `Inbox.receiveMessage` via alloy; validated end-to-end on anvil |
| Sui destination submit | ⛔ blocked on the Layer-2 Locker (see below) |
| Dry-run submit (`submit.rs`) | fallback when no EVM destination is configured |

**EVM submitter** (`EvmDestSubmitter`): builds the `CrossChainMessage` +
`SignatureEnvelope` ABI structs, calls `Inbox.receiveMessage`, and reads
`Inbox.consumed(hash)` for dedup. Validated against a local anvil: a group-signed
Sui→EVM message verified, dispatched to a `MockRecipient` (`calls()==1`, correct
payload), and was marked consumed. Enable it by setting `evm_*` in the config.

**Sui submitter** is blocked by design: the Sui `Inbox.receive` returns a hot
potato that only the destination app can discharge via `consume(&UID)` (it must
pass its own object's UID). A generic relayer can't supply that — delivery must
route through the destination app's own entry function (the Locker's
`on_receive`). So the Sui-destination submitter lands with the Layer-2 Locker.

## Live smoke test (2026-06-29, deployed testnet contracts)

Drove a real `Outbox.send` on the deployed Sui Outbox (`0x3989143a…`) and fed the
resulting on-chain `MessageCommitted` through the decode → sign path against the
live signer node. Results:
- Reconstructed message hash **matched the on-chain committed hash** exactly.
- The live signer returned an ECDSA envelope (group key id 1) over that digest —
  a signature the deployed HyperEVM Inbox accepts via `ecrecover`.

Two bugs the live run caught and fixed:
1. **Event filter** — `suix_queryEvents` `MoveModule` matches the *emitting*
   module, not `events`; switched to `MoveEventModule` (matches by the module
   that defines the event type). The decoder's array-of-numbers handling was
   already correct for the real RPC `parsedJson`.
2. **HTTP client** — the `sui-sdk` jsonrpsee transport stalled on `rpc.discover`
   against proxied fullnodes; replaced with a raw-JSON-RPC `reqwest` watcher
   (IPv4-pinned).

> Environment caveat: in this sandbox, `reqwest`'s outbound HTTPS to public
> fullnodes hangs after connect (while `curl` succeeds), so the relayer binary's
> live fetch could not complete here. The watch → decode → sign path was
> validated with the real on-chain event via the working tools; the relayer code
> itself is unit-tested and unchanged by that caveat.

## Tests

`cargo test -p bridge-relayer` covers the event decoder (both Sui JSON shapes +
hash-mismatch rejection) and the relay orchestration (a known message flows
through to a recorded submission carrying the exact on-chain-valid signature,
then dedups on the second pass).
