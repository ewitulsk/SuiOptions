# sui-bridge-contracts/solidity

HyperEVM (Solidity) side of **Layer 1 — Generic Cross-Chain Messaging** from
[`../../bridge-spec.md`](../../bridge-spec.md). Foundry project. Mirrors the Move
package in [`../sui`](../sui): an Outbox that commits canonical messages and an
Inbox that verifies an aggregated threshold **ECDSA** signature (`ecrecover`)
and dispatches to the destination app.

```
cd sui-bridge-contracts/solidity
git clone --depth 1 https://github.com/foundry-rs/forge-std lib/forge-std   # if lib/ is absent
forge build && forge test
```

## Contracts

| File | Responsibility |
|------|----------------|
| [`libraries/ChainId.sol`](src/libraries/ChainId.sol) | `(family << 27) \| local` internal chain id — identical to the Move `chain_id` |
| [`libraries/Message.sol`](src/libraries/Message.sol) | `CrossChainMessage` + the big-endian packed canonical encoding + `keccak256` digest |
| [`libraries/Envelope.sol`](src/libraries/Envelope.sol) | `SignatureEnvelope` + `ecrecover` verify adapter (secp256k1, malleability-checked) |
| [`Registry.sol`](src/Registry.sol) | chains + group keys + governance/guardian roles |
| [`Outbox.sol`](src/Outbox.sol) | `send` → nonce + `MessageCommitted`, pausable |
| [`Inbox.sol`](src/Inbox.sol) | `receiveMessage` → verify + exactly-once + `onReceive` dispatch, pausable |
| [`interfaces/IMessageRecipient.sol`](src/interfaces/IMessageRecipient.sol) | destination-app `onReceive` callback |
| [`script/Deploy.s.sol`](script/Deploy.s.sol) | deploy + wire registry/group key on HyperEVM |

## Parity with the Sui side (the contract that matters)

The two chains must produce a **byte-identical keccak256 digest** so one
threshold signature verifies on both. `test_known_digest_matches_sui` hashes the
exact message that `sui_bridge::message_tests::known_digest_vector` does and
asserts the same digest — if either encoding drifts, this test breaks. Both
sides sign the raw 32-byte digest (no EIP-191 prefix; Ed25519 over the digest on
Sui, `ecrecover` over the digest here).

## Notable differences from the Move side

- **Dispatch is a direct call**, not a hot potato — the EVM has dynamic
  dispatch. `consumed[hash]` is set *before* the external `onReceive`
  (checks-effects-interactions), so a reentrant replay reverts.
- **Verify is ECDSA/`ecrecover`** (GG20/CGGMP, EVM-destined) vs FROST-Ed25519 on
  Sui. The registered group key is the 20-byte group address.

## Open items (flagged in the spec)

- `evm_version = "paris"` in `foundry.toml` is conservative pending confirmation
  of HyperEVM's opcode support and finality semantics (spec §4/§9).
- `HYPER_CONFIRMATIONS` default (12) is a placeholder — HyperBFT finality must be
  confirmed against Hyperliquid docs before fixing it.
