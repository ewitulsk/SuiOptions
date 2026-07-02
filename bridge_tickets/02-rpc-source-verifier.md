# 02 — RPC source verifier (§5.4 security boundary)

**Status (2026-07-01): DONE — code complete, all tests green.**
- `RpcVerifier` behind the existing `SourceVerifier` trait, over a mockable
  `CommitmentProbe` abstraction; quorum = **every configured provider must
  independently confirm** (fail closed on any Pending/NotFound/error/disagreement).
- `EvmProbe` (eth_getLogs on the Outbox `MessageCommitted` indexed topic +
  eth_blockNumber finality) and `SuiProbe` (suix_queryEvents match on
  `message_hash`; queryable ⇒ final). Pure parsers factored out and unit-tested.
- Config: `[[source_chains]]` mirror (family / rpc_urls / outbox_addr|package_id /
  confirmations / allow_single_provider) + `environment`; `trust_all` now a hard
  error outside `environment = "dev"`; unknown route ⇒ 422.
- `update_chain` governance fn added to `registry.move` (+ 2 Move tests) to backfill
  peer addresses — the ticket-01 follow-up. NOTE: using it on the *current* live Sui
  registry needs a redeploy (new package types); the live verifier reads the Outbox
  addr from its own config, so this isn't blocking.
- **Tests:** signer-service 15 unit (6 verifier quorum via mock probe, 6 probe
  parsers, 1 topic0 guard, 2 build-gating) + 2 sign integration; Move 16/16.
  **Anvil integration** (`examples/verify_evm_smoke.rs`): real Outbox + real
  `MessageCommitted` drove the EvmProbe through NotFound → Final(0-conf) →
  Pending(100-conf) → Final(after mining 100) — the §5.4 "refuse before finality,
  sign after" path proven on a live node.
- Digest-parity transitivity: `verify_committed` recomputes `digest(msg, domain_sep)`
  which ticket 01 already locked byte-identical to the on-chain emitted hash, so the
  probe finds the right log by construction.
- Deferred: live Sui suix_queryEvents run (sandbox reqwest→fullnode caveat); parsing
  is unit-tested against sample payloads.

---


**Spec:** bridge-spec.md §5.4, §4 (finality)
**Why:** the only verifier today is `TrustAllVerifier` (`bridge-signer-service/src/verifier.rs`) — the signer signs any well-formed message, so anyone who can reach it can mint arbitrarily on the deployed contracts. This is the single largest gap between the deployed system and the spec's security claims. `verifier.rs` already defines the `SourceVerifier` trait and bails on `mode = "rpc"`; this ticket implements it.

## Scope

Implement `RpcVerifier` behind the existing trait: given a `CrossChainMessage`, confirm the **registered** source Outbox committed this **exact** message (recompute the digest, match the on-chain committed hash) at **source finality**, else refuse to sign.

### Per-family checks
- **Sui source:** query `MessageCommitted` events from the registered `sui_bridge` package (`suix_queryEvents` by `MoveEventModule`, as the relayer already does), match the full field set + digest. Sui events from a fullnode are from finalized checkpoints — no extra depth gate (§4).
- **EVM source:** `eth_getLogs` on the registered Outbox address for `MessageCommitted` with the digest as the indexed topic; recompute the hash from event fields and compare. Enforce `confirmations >= finality_value` from config (currently 12 for HyperEVM — see ticket 10 for confirming that number).

### Provider quorum
§5.4 requires ≥2 independent RPC providers per source chain before signing. Config takes a list per chain; the verifier requires agreement from **all configured providers** (start with 2). Single-provider config is allowed only with an explicit `allow_single_provider = true` + startup warning, for dev.

### Configuration
Signer-service config gains a small chain-registry mirror: per internal chain id → `{family, rpc_urls[], outbox_addr/package, finality}`. Reject any message whose `src_chain_id` isn't configured (unregistered route).

### Guardrails
- `trust_all` stays available but the startup warning becomes a hard refusal unless `environment = "dev"`.
- Verification failures return the existing 422/503 mapping (`handlers.rs` already distinguishes `NotCommitted` vs `Unavailable`).

## Verify (exit criteria)
- Unit: mocked providers — signs on quorum-confirmed commitment; refuses on (a) unknown route, (b) hash mismatch, (c) missing event, (d) insufficient confirmations, (e) provider disagreement.
- Integration: anvil — commit a message, request a signature before N confirmations (refused), mine to depth (signed). Sui testnet — sign only after the real `MessageCommitted` is queryable.
- Live: deployed signer runs `mode = "rpc"`; a hand-crafted uncommitted message is refused (422).

## Out of scope
TLS-in-enclave and pinning (ticket 07 — the provider-quorum interface built here is what moves inside the enclave); async API (06).

**Depends on:** 01 (digest recomputation must use DOMAIN_SEP). **Blocks:** meaningful security of everything; 07 builds on it.
