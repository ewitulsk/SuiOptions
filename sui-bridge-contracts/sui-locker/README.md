# sui-bridge-contracts/sui-locker

Sui (Move) side of **Layer 2 — the lock-and-mint Locker** (bridge-spec.md §3),
one deployment per asset, built on the L1 messaging package ([`../sui`](../sui)).

```
cd sui-bridge-contracts/sui-locker && sui move test
```

## Modules

| Module | Responsibility |
|--------|----------------|
| [`transfer_payload.move`](sources/transfer_payload.move) | `TransferPayload{asset_id, amount, recipient}` codec — 72-byte fixed layout, byte-identical to the Rust + Solidity codecs |
| [`locker.move`](sources/locker.move) | `Locker<T>` (escrow vault / wrapped `TreasuryCap`), `bridge_out`, the `bridge_receive` convention entry, peer/pause/rate-limit governance |

## Design

- **`Locker<T>` with a `Vault` enum:** `Escrow(Balance<T>)` on the home chain,
  `Mint(TreasuryCap<T>)` on the foreign chain — one type, mode at creation.
- **Inbound via the `bridge_receive` convention** (see
  [`../relayer-dispatch-design.md`](../relayer-dispatch-design.md)): the relayer
  reads the Locker object's type to learn `(package, module, T)` and calls
  `bridge_receive`, which drives `inbox::receive` + `consume(&self.id)` and then
  releases (home) or mints (foreign). No app-specific relayer needed.
- **Safety on every inbound:** `src_app` must equal the registered peer,
  `asset_id` must match, a windowed rate limit caps minted/released volume, and
  amounts are scaled between local decimals and the shared wire precision
  (`WIRE_DECIMALS = 8`), rejecting dust (NTT trimmed-amount).

The supply invariant `wrapped_supply ≤ locked_collateral` holds by construction:
foreign mint only on a delivered message, home release only on a delivered
message, each consumed exactly once by the L1 Inbox.
