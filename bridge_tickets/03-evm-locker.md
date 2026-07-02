# 03 — EVM Locker (lock-and-mint app, HyperEVM side)

**Spec:** bridge-spec.md §3
**Why:** the Sui Locker exists (`sui-bridge-contracts/sui-locker/`); the EVM side has only the `TransferPayload` library. Without it there is no home-chain escrow and no M2 round trip.

## Scope

`Locker.sol` — one deployment per asset, mirroring the Sui Locker's semantics:

- **Modes:** home = escrow vault (ERC-20 `safeTransferFrom` in, transfer out); foreign = wrapped ERC-20 with mint/burn rights held by the Locker. Include a minimal `WrappedToken.sol` (ERC-20, owner-mint/burn) for the foreign case.
- **Outbound** `lock(amount, dstChainId, recipient32)` (home) / `burn(...)` (foreign): escrow-or-burn, encode via the existing `TransferPayload` library (same wire format as `locker::transfer_payload` on Sui, including wire-decimals scaling with dust rejection — mirror `to_wire`/`from_wire` from `locker.move`), then `Outbox.send(dstChainId, peer, payload)`. `src_app` = the Locker's address (Outbox already records `msg.sender`).
- **Inbound** `onReceive(srcChainId, srcApp, payload)`: `require msg.sender == inbox`; `require srcApp == peers[srcChainId]`; decode payload; assert asset id; scale from wire decimals; release (home) or mint (foreign).
- **Rate limit with overflow queue (§3.5, build it right the first time — greenfield):** windowed cap in wire units; over-limit transfers are enqueued `{recipient, amount, unlockAt}` and **never revert**; permissionless `claim(queuedId)` after the window. Emit events for enqueue/claim.
- **Admin:** `setPeer(chainId, addr32)`, `setPaused(bool)`, `setRateLimit(window, cap)` — owner/guardian per the Registry's existing role pattern.

## Wiring
- Deploy script additions: locker deploy + peer registration both directions (EVM Locker ↔ Sui Locker object id).
- Pick/mint a test asset on HyperEVM testnet (home) and publish the matching wrapped coin package + `create_mint_locker` on Sui (the Sui side already supports this — onboarding = package publish, per §3.1).

## Verify (exit criteria)
- Forge tests: escrow/mint/burn/release paths; only-Inbox and peer checks; decimals scaling parity vectors against the Sui `transfer_payload` tests (shared test vectors, same discipline as the message-digest vector); rate-limit enqueue + claim; pause.
- Supply invariant test: wrapped minted on foreign ≤ escrowed on home across a scripted sequence.
- Testnet: deploy + peer-wire against the live Sui Locker.

**Depends on:** 01 (deploys against the new digest contracts). **Blocks:** 04 (round trip needs both lockers).
