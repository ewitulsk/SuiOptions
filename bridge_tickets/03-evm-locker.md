# 03 — EVM Locker (lock-and-mint app, HyperEVM side)

**Status (2026-07-01): DONE — code complete, all tests green, deploy validated on anvil.**
- `Locker.sol` (escrow/mint modes) + `WrappedToken.sol` (minimal owner-mint/burn ERC-20).
- Outbound `lock`/`burn` (mode-checked) → NTT decimals scaling with dust rejection →
  `TransferPayload.encode` (shared 72-byte wire format) → `Outbox.send`.
- Inbound `onReceive` (only-Inbox, peer + asset checks) → scale from wire → release/mint.
- **Rate-limit overflow queue built in from day one** (§3.5): over-cap inbound transfers
  enqueue `{recipient, wireAmount, unlockAt}` and NEVER revert; permissionless `claim`
  after the window; double-claim guarded. (The Sui side still reverts — ticket 05.)
- `transferAdmin` for governance handoff; `DeployLocker.s.sol` (both modes, wraps +
  ownership transfer + peer wiring), refactored to dodge script stack-too-deep.
- **Tests — 16 forge Locker tests, all green:** escrow-in/scale, dust reject, wrong-mode,
  unknown-peer, burn-scale (6-dec), release-escrow (18-dec), mint-foreign, only-Inbox,
  peer/asset mismatch, rate-limit queue+claim (+StillLocked +double-claim), pause in/out,
  admin-gating, transferAdmin handoff, **supply-invariant round trip**, and
  **end-to-end through the real Inbox** (ECDSA verify → dispatch → mint). Full suite 41/41;
  Sui-locker 10/10 unaffected.
- **Anvil deploy validated:** `DeployLocker` (Mint) deployed WrappedToken + Locker, handed
  ownership to the Locker, wired the peer — verified via `cast`.
- Deferred to ticket 04: live testnet Locker deploy + the HyperEVM↔Sui round trip (needs a
  live Sui Locker instance + the EVM→Sui relayer; that's where the round-trip exit lives).

---


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
