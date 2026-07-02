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
| 01 | [domain-separator](01-domain-separator.md) | M1 gap | `DOMAIN_SEP` in the digest, all 3 impls + redeploy — blocks everything else |
| 02 | [rpc-source-verifier](02-rpc-source-verifier.md) | M1 gap | Replace `TrustAllVerifier`: sign only Outbox-committed-at-finality messages (biggest live security hole) |
| 03 | [evm-locker](03-evm-locker.md) | M2 | `Locker.sol` escrow/wrapped + queue-on-rate-limit, peer-wired to the Sui Locker |
| 04 | [relayer-evm-to-sui](04-relayer-evm-to-sui.md) | M1/M2 | EVM source watcher + type-derived generic Sui submitter → full round trip |
| 05 | [sui-rate-limit-queue](05-sui-rate-limit-queue.md) | M2 | Retrofit Sui Locker: over-limit transfers queue + permissionless claim, never revert |
| 06 | [async-signing-api](06-async-signing-api.md) | pre-M3 | `POST /sign_requests` + poll-by-hash, idempotent sessions, verify-before-admit |
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
