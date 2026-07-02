# 07 — Nautilus enclave: run the signer inside AWS Nitro

**Spec:** bridge-spec.md §5.1–§5.4 · **Milestone:** M3 entry · **Status:** not started (needs hardware)
**Why:** today `bridge-signer-service` is a plain process with curve seeds in config. The spec's trust model requires it to run inside an attested AWS Nitro enclave so that (a) only approved code, registered on-chain, can produce signatures, and (b) the enclave's *chain view* — the §5.4 "was this committed at finality" check — can't be forged by the untrusted host. This ticket is the enclave migration only; Seal-based key provisioning is ticket 08 (this one can boot with a config-injected seed for staging so the two are independently testable).

**Hard prerequisite:** a Nitro-Enclave-capable EC2 instance (`m5.xlarge`/`c5.xlarge` or larger, enclave option enabled) with `nitro-cli`. None of this is doable in the dev sandbox.

---

## Background you need before starting

- A Nitro enclave is a stripped VM with **no network, no persistent storage, no interactive access**. Its only I/O is a **vsock** channel to the parent EC2 instance. Everything the enclave reaches (RPC, Seal servers) is forwarded by the parent over vsock — which is why the parent is untrusted and TLS must terminate *inside* the enclave.
- **Attestation:** the Nitro Security Module (NSM) produces a COSE_Sign1/CBOR **attestation document** signed up to the AWS Nitro root CA, containing the enclave's measurements (**PCR0** = image, **PCR1** = kernel, **PCR2** = app) and a caller-supplied `public_key` field (we put the enclave's boot-fresh ephemeral pubkey there).
- `MystenLabs/nautilus` gives us: the reproducible EIF build template, `src/nautilus-server` (an axum app with `/get_attestation`), the vsock traffic-forwarder, `allowed_endpoints.yaml`, and `move/enclave/sources/enclave.move` which **verifies that attestation on-chain** and registers an `Enclave` object.

## Detailed implementation plan

### Phase 1 — Fork & port the signer into the nautilus app (no attestation yet)
1. Fork `MystenLabs/nautilus`; pin the commit. Track our delta in a `FORK_DELTA.md` for the ticket-10 audit (upstream is explicitly unaudited).
2. Move `bridge-signer-service` into `src/nautilus-server/src/apps/bridge-signer/`. The axum public router (ticket 06: `/sign_requests`, `GET /sign_requests/:hash`, `/get_attestation`, `/health`) becomes the enclave's public surface; admin routes (Seal load, ticket 08) bind to the vsock-local admin port only.
3. **Egress rework (the real work):** the signer's `EvmProbe`/`SuiProbe` (ticket 02) currently make direct `reqwest` calls. Inside the enclave there is no direct socket — route all outbound HTTPS through the vsock proxy, with **rustls terminating inside the enclave** so the parent forwards ciphertext only. Concretely: a custom `reqwest` connector (or `hyper` client) whose transport is vsock→parent-forwarder→TCP, wrapping the stream in an in-enclave rustls `ClientConnection` pinned to the configured provider certs. `SuiClientBuilder` (SuiDestSubmitter) needs the same treatment or gets replaced by raw JSON-RPC over the vsock transport.
4. `allowed_endpoints.yaml` = the RPC hostnames (Sui fullnode(s), HyperEVM RPC(s), later Seal key servers). The parent's forwarder only dials these.

### Phase 2 — Reproducible build & PCR measurement
1. Build the EIF via the nautilus reproducible Dockerfile: pinned Rust toolchain, pinned base image digest, `--frozen` cargo, no build timestamps. Goal: **bit-identical EIF → identical PCR0** across machines.
2. `nitro-cli build-enclave` → capture PCR0/1/2. Commit the expected PCRs.
3. **CI job:** rebuild the EIF on a clean runner and assert PCR0 matches the committed value. A drift here means the on-chain `EnclaveConfig` would reject the real enclave — catch it in CI, not at deploy.

### Phase 3 — On-chain enclave registry (`enclave.move`)
1. Vendor/port nautilus `enclave.move` into our Sui deployment. It: verifies the attestation's COSE signature chain to the AWS Nitro root cert (stored in the Sui framework), checks PCR0/1/2 against an `EnclaveConfig`, and extracts the `public_key` field.
2. `EnclaveConfig` (shared, governance-updatable PCRs) + `register_enclave(config, attestation) -> Enclave` creating a per-instance `Enclave` object holding the attested ephemeral pubkey.
3. **Per-node operator cap:** registration/update of node *i*'s `Enclave` object requires operator *i*'s cap. This cap is the per-node credential the ticket-08 Seal policy binds to (spec §6.5). Custody decision → ticket 10.
4. `update_enclave` for re-registration after a restart (new ephemeral key) — governance/operator gated, emits an event (alertable, ticket 10).

### Phase 4 — Chain view inside the boundary (§5.4)
1. The ticket-02 `RpcVerifier` runs *in-enclave* with the ≥2-provider quorum, over the in-enclave-TLS transport from Phase 1. Carry forward the two live-run fixes: **bounded `eth_getLogs` lookback** (public RPCs cap the range) and tolerance for public-RPC 500s (retry/failover across the quorum providers).
2. Provider certs pinned in the enclave image (part of PCR measurement) so a swapped provider changes the PCRs.
3. Document the TCB honestly: at N=1 the enclave + its (pinned, in-enclave-verified) chain view is the trust root; the k-of-n guarantee arrives at ticket 09.

### Phase 5 — Infra & lifecycle
1. Terraform: Nitro-enabled EC2 (`options-*` host conventions), enclave allocator (CPU/mem carve-out), **a new ECR repo in `ecr.tf`** for the server image (per the repo's redeploy gotchas — missing repo → 403 push).
2. Lifecycle scripts: `build` (EIF + PCRs), `run` (`nitro-cli run-enclave`), the parent-side vsock forwarder + admin proxy, `attach-console` for debug builds only.
3. Boot flow: enclave starts → generates ephemeral Ed25519 key in-memory → `GET /get_attestation` returns the doc with that pubkey → operator calls `register_enclave` on-chain with their cap → signer flips to "ready".

## Exit criteria
- Enclave runs on a Nitro EC2; `/get_attestation` returns a doc whose PCRs match the reproducible build and whose signature chain **verifies on-chain** via `register_enclave`.
- The signer **refuses `/sign_requests` until its ephemeral key is registered on-chain** (gate on the `Enclave` object existing).
- RPC egress reaches only allow-listed providers; a MITM'd provider (test harness presenting a wrong cert) fails in-enclave TLS pinning and **blocks signing** rather than yielding a forged chain view.
- End-to-end: a live testnet message is signed from inside the enclave and delivered both directions (re-run the ticket-04/round-trip flow with the enclave as signer).

## Effort & sequencing
Largest single ticket in the M3 track. Rough phases: P1 (port + egress rework) ~1–2 wk — the in-enclave TLS transport is the crux; P2 (reproducible build) ~few days incl. CI; P3 (enclave.move) ~1 wk; P4 ~few days; P5 (infra) ~1 wk. Do P1–P2 before touching Move; P3 can proceed in parallel.

**Depends on:** 02 (verifier runs in-enclave), 06 (async API is the enclave surface). **Blocks:** 08, 09.
