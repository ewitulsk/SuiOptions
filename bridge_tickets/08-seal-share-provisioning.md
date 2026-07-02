# 08 — Seal key provisioning with per-node policy binding

**Spec:** bridge-spec.md §5.2–§5.3, §6.4, **§6.5 (normative), §6.6** · **Milestone:** M3 · **Status:** not started (needs 07)
**Why:** signing keys are seeds in config today. Stateless enclaves (no persistent storage) mean a restart loses in-memory keys — so keys must be reloadable from **Seal** without any plaintext ever leaving the enclave. And the stock Nautilus Seal example gates decryption on **PCRs only**, which collapses k-of-n: every signer runs identical code → identical PCRs → any one operator's attested enclave could decrypt *every* node's share. This ticket implements the **per-node** policy (§6.5) and the 2-step in-enclave key load.

**Recall the mechanics (verified against Seal docs + `seal_policy.move`):** Seal is Boneh-Franklin IBE on BLS12-381; identities are namespaced by the policy package id; `t`-of-`n` key servers each return an IBE-derived key share iff the package's `seal_approve*` Move function passes; the enclave can't reach the key servers directly (no egress) so the fetch is host-delegated in two steps, and responses are encrypted to the enclave's ephemeral ElGamal key so the host can't read them.

---

## Detailed implementation plan

### Phase 1 — The per-node Seal policy package (Move) — deliberately NOT the stock example
The stock `seal_policy.move` checks: `id == vector[0]`, sender == wallet pk, and an intent signature against **whichever `Enclave<T>` object the caller passes**. That authorizes *any* attested instance — fine for one shared secret, unsafe for per-node shares. Ours:

```move
// identity of share i = the 32-byte object id of node i's Enclave object.
entry fun seal_approve(
    id: vector<u8>,            // the requested identity (= node's Enclave id)
    signature: vector<u8>,
    wallet_pk: vector<u8>,
    timestamp: u64,
    enclave: &Enclave<BRIDGE_SIGNER>,
    ctx: &TxContext,
) {
    // (a) keep the example's three checks: sender == pk_to_address(wallet_pk),
    //     and an Ed25519 intent signature verifies against enclave.pk().
    // (b) ADD the binding: this share is decryptable only by THIS node.
    assert!(object::id_to_bytes(&object::id(enclave)) == id, ENoAccess);
}
```

- **Publish the policy package IMMUTABLE** (or under a governance-only upgrade cap). Seal docs: an upgradeable package's owner can rewrite the access policy at any time.
- Test matrix is the point: (1) correct node + correct enclave → pass; (2) **node B's enclave requesting node A's identity → refused** (the k-collapse guard); (3) wrong sender, (4) bad/absent intent signature, (5) stale timestamp → all refused.

### Phase 2 — Enclave-side 2-step key load (implement the ticket-06 admin stubs)
Today `router.rs` returns 501 for `/admin/init_seal_key_load` and `/admin/complete_seal_key_load`. Implement:
1. `init_seal_key_load` → the enclave generates an ephemeral **ElGamal** keypair (BLS group elements), builds a `FetchKeyRequest` = { the `seal_approve` PTB signed by the enclave wallet, the ElGamal pubkey }, returns it (BCS/hex) to the host.
2. Host helper POSTs the `FetchKeyRequest` to each configured Seal key server (`/v1/fetch_key`); each server dry-runs `seal_approve` and, if it passes, returns its key **share encrypted to the ElGamal pubkey**.
3. `complete_seal_key_load` ← the host hands the (still-encrypted) server responses back in; the enclave ElGamal-decrypts them, IBE-combines the `t` shares into the derived key, and uses it to decrypt the node's signing material. **All plaintext stays in enclave memory.**
- Key server set + threshold from config: **Mysten open testnet servers, t=1** acceptable for testnet; the mainnet set/threshold is a ticket-10 decision (§6.6).

### Phase 3 — What gets provisioned, and recovery
1. **Ciphertext creation (one-time per node):** encrypt node *i*'s signing material to identity `[Enclave_i object id]` under the policy package. At M1-in-enclave that's the two curve seeds; at M3 (ticket 09) it's the DKG *shares* — same mechanism, different payload. Store ciphertexts anywhere (Walrus, S3, repo-adjacent): they're small and useless without both the policy AND that node's enclave.
2. **Restart:** enclave boots → re-run the 2-step load → shares back in memory. Nothing regenerated; no new DKG.
3. **Replacement / hardware loss (§6.4):** provision a fresh enclave with identical PCRs → it has a *new* ephemeral key → operator calls `update_enclave` (ticket 07) with their cap to register the new pubkey into node *i*'s `Enclave` object → key load now passes (identity still = that `Enclave` object id). **Identical PCRs alone are deliberately insufficient** — the explicit, on-chain-visible re-registration is the point.
4. **Key-server rotation:** the server set is frozen per ciphertext, so rotating servers = re-encrypt each node's material to the new set (cheap — the payload is tiny). This is the envelope-encryption pattern Seal itself recommends.

## Exit criteria
- Policy tests green, **including the cross-node negative test** (node B cannot fetch node A's share) and the wrong-sender / bad-signature / stale-timestamp refusals.
- On testnet: kill an enclave, restart it, and it recovers its signing keys via the 2-step Seal load with **zero plaintext key material anywhere outside enclave memory** (config seeds removed from the deployment entirely).
- Replacement drill: destroy the instance, provision fresh, operator re-registers the `Enclave` object, key load succeeds, signing resumes — timed and runbooked.
- An alert (`alert_id`, repo convention) fires on any `Enclave` object re-registration (anomalous ones are an attack signal).

## Effort & sequencing
~1–1.5 wk once 07 exists. Phase 1 (policy + tests) can start on a laptop against a local Sui + the Seal Move libs before the enclave is ready; Phases 2–3 need the running enclave. The k-collapse negative test is the deliverable that most matters for the trust model.

**Depends on:** 07 (enclave + `Enclave` objects + operator caps). **Blocks:** 09 (DKG shares are provisioned via this mechanism).
