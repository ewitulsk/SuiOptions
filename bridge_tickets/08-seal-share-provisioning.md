# 08 — Seal key provisioning with per-node policy binding

**Spec:** bridge-spec.md §5.2–§5.3, §6.4, **§6.5 (normative policy), §6.6**
**Why:** signing keys currently live as seeds in config. The spec's lifecycle story (stateless enclaves; restart → reload from Seal, no plaintext outside the enclave) requires Seal-encrypted provisioning — and the stock Nautilus example policy is PCR-only, which collapses k-of-n (any operator's attested enclave could decrypt every node's share). This ticket implements the per-node policy and the 2-step key load.

## Scope

### 1. Seal policy package (Move) — per §6.5, deviating from the stock example deliberately
- Identity per secret = `[node_enclave_object_id]` (NOT the example's fixed `vector[0]`).
- `seal_approve(id, signature, wallet_pk, timestamp, enclave: &Enclave<BRIDGE_SIGNER>, ctx)` keeps the example's three checks (intent signature against `enclave.pk()`, sender == wallet pk hash) **and additionally asserts `object::id(enclave) == id`** — one share, one node.
- **Publish immutable** (or under governance): an upgradeable policy package can be rewritten by its owner at any time (Seal docs).
- Negative-path tests are the point: a second enclave with identical PCRs but a different `Enclave` object must be refused.

### 2. Enclave-side 2-step key load (Nautilus-Seal pattern)
Implement the stubbed admin endpoints (`router.rs` 501s): `init_seal_key_load` (build `FetchKeyRequest`: ephemeral ElGamal key + signed `seal_approve` PTB) and `complete_seal_key_load` (decrypt responses in-enclave, cache keys in memory only). Host-side helper fetches from the key servers (enclave has no direct egress). Mysten open testnet servers, threshold from config (t=1 acceptable on testnet; §6.6 decision for mainnet in ticket 10).

### 3. Provisioning + recovery flow
- Encrypt each node's signing material (M1: the two curve seeds; M3: the DKG shares — same mechanism, ticket 09) to that node's identity; store ciphertexts (Walrus or repo-adjacent — they're small and useless without the policy).
- Restart: re-run the 2-step load; nothing new to generate.
- Replacement (§6.4): new instance → operator re-registers the fresh ephemeral pubkey into the node's `Enclave` object via the operator cap (explicit, on-chain-visible, alertable) → key load succeeds. Runbook this.
- Key-server rotation = re-encrypt ciphertexts to the new server set (the set is frozen per ciphertext).

## Verify (exit criteria)
- Policy unit tests incl. the cross-node negative test above and the wrong-identity/wrong-sender/bad-signature refusals.
- Enclave restart on testnet recovers signing keys via Seal with zero plaintext key material outside enclave memory (config seeds deleted from the deployment).
- Replacement drill: kill the instance, provision fresh, operator re-registration, key load, signing resumes.
- Alert fires (repo alert_id convention) on any `Enclave` object re-registration.

**Depends on:** 07 (enclave + `Enclave` objects). **Blocks:** 09 (shares are Seal-provisioned).
