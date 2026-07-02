# sui-bridge-contracts

On-chain contracts for the cross-chain bridge ([`../bridge-spec.md`](../bridge-spec.md)),
one subfolder per chain family.

| Folder | Stack | Status |
|--------|-------|--------|
| [`sui/`](sui) | Move (`sui move build` / `sui move test`) | Layer 1 messaging — implemented + deployed (testnet) |
| [`solidity/`](solidity) | Foundry (HyperEVM) | Layer 1 messaging — implemented + deployed; Layer 2 `TransferPayload` codec |
| [`sui-locker/`](sui-locker) | Move | Layer 2 Locker (lock-and-mint) — implemented |

Both sides must agree on the **canonical message encoding** and **keccak256
digest** defined in [`sui/sources/message.move`](sui/sources/message.move) so a
single threshold signature verifies on either chain — the Solidity `Outbox`/
`Inbox` will reproduce that exact byte layout and the
`(family << 27) | chain_id` internal-id scheme from
[`sui/sources/chain_id.move`](sui/sources/chain_id.move).
