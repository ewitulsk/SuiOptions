# 02 — RPC source verifier (§5.4 security boundary)

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
