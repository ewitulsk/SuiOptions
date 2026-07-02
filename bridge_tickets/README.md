# Bridge tickets

Work remaining to complete the cross-chain messaging layer + lock-and-mint bridge, per `bridge-spec.md` v0.2 and the 2026-07-01 code audit. One file per ticket; each has scope, verify/exit criteria, and dependencies.

## Order & dependencies

```
01 domain-separator ──► 02 rpc-source-verifier ──► 06 async-signing-api ──► 09 threshold-signing-dkg ──► 10 lifecycle-hardening
        │                        │                        ▲                          ▲
        ├──► 03 evm-locker ──► 04 relayer-evm-to-sui      │                          │
        │            │                                     │                          │
        │            └──► 05 sui-rate-limit-queue          │                          │
        │                                                  │                          │
        └──────────────────► 07 nautilus-enclave ─────────┴──► 08 seal-share-provisioning
```

| # | Ticket | Spec milestone | One-liner |
|---|--------|----------------|-----------|
| 01 | [domain-separator](01-domain-separator.md) | M1 gap | ✅ **DONE** — `DOMAIN_SEP` in the digest, all 3 impls, both chains redeployed live, parity verified |
| 02 | [rpc-source-verifier](02-rpc-source-verifier.md) | M1 gap | ✅ **DONE** — `RpcVerifier` (all-provider quorum, EVM+Sui probes), anvil-verified; `trust_all` dev-gated |
| 03 | [evm-locker](03-evm-locker.md) | M2 | ✅ **DONE** — `Locker.sol` escrow/mint + `WrappedToken`, queue-on-rate-limit, 16 tests + anvil deploy |
| 04 | [relayer-evm-to-sui](04-relayer-evm-to-sui.md) | M1/M2 | ✅ **DONE** — EVM watcher + type-derived Sui submitter + BCS layer + router; **live HyperEVM→Sui→HyperEVM round trip on testnet** (M2 exit) |
| 05 | [sui-rate-limit-queue](05-sui-rate-limit-queue.md) | M2 | ✅ **DONE** — Sui Locker queues over-limit transfers + permissionless claim, matches EVM; 13 tests |
| 06 | [async-signing-api](06-async-signing-api.md) | pre-M3 | ✅ **DONE** — `POST /sign_requests` + poll-by-hash, idempotent sessions, verify-before-admit, per-IP limit; live-smoked |
| 07 | [nautilus-enclave](07-nautilus-enclave.md) | M3 | Signer inside AWS Nitro: attestation on-chain, TLS-in-enclave chain view |
| 08 | [seal-share-provisioning](08-seal-share-provisioning.md) | M3 | Per-node Seal policy (§6.5) + 2-step key load; restart/replacement recovery |
| 09 | [threshold-signing-dkg](09-threshold-signing-dkg.md) | M3 | FROST + ECDSA-MPC libs, dual DKG, attested libp2p mesh, N=3 k=2 |
| 10 | [lifecycle-hardening](10-lifecycle-hardening.md) | M4 | Governance multisigs, HyperEVM finality confirmation, runbooks, alerting, audit |

## Already done (for context)

M0 + most of M1: three-way parity message format, Sui Outbox/Inbox with hot-potato delivery (`consume(&UID)`), Solidity Outbox/Inbox with callback delivery, chain/group-key registries with rotation, Sui Locker (escrow/mint, peers, decimals normalization), 1-of-1 signer service, Sui→EVM relayer — deployed to Sui + HyperEVM testnets 2026-06-29 (`sui-bridge-contracts/DEPLOYMENTS.md`).

## Conventions

- Tickets 01+02 before anything else touches contracts or accumulates signed traffic.
- Every ticket's exit criteria include tests; cross-language behavior (digest, payload wire format, queue semantics) uses shared test vectors in all implementations, same discipline as the existing known-digest vector.
- Tx-submitting services follow `.claude/tx-alerting.md` (`alert_id` on submission failures).
