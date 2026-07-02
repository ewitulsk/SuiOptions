# 07 — Nautilus enclave: run the signer inside AWS Nitro

**Spec:** bridge-spec.md §5.1–§5.4
**Why:** the signer is a plain service with seeds in config. The spec's M3/M4 trust model requires the signer to run inside an attested Nautilus enclave, with its chain view protected from the untrusted host. This ticket is the enclave migration; key provisioning via Seal is ticket 08 (keep them separable — this one can run with config keys injected at boot for staging).

## Scope

### 1. Fork + port
Fork `MystenLabs/nautilus`; port `bridge-signer-service` into the nautilus-server app slot (the spec names `apps/seal-example` as the base). Public port 3000: `/get_attestation` (real), `/sign_requests` (from ticket 06), `/health`. Admin on 3001, host-localhost only.
⚠️ Upstream template is explicitly unaudited ("evaluation purposes only") — track our fork's delta for the ticket-10 audit.

### 2. Reproducible build + PCRs
Reproducible enclave image build; record PCR0/1/2; CI job that rebuilds and asserts PCR stability across builds.

### 3. On-chain enclave registry
Add the `enclave.move` pattern (attestation verification against the AWS root cert in the Sui framework) to our Move deployment: `EnclaveConfig` (PCRs, governance-updatable) + per-node `Enclave` objects registering each instance's boot-fresh ephemeral pubkey. Registration is performed with a per-node **operator cap** — this cap is the per-node credential (spec §6.5); custody decision in ticket 10.

### 4. Chain view inside the boundary (§5.4)
- All RPC egress through `allowed_endpoints.yaml`, with **TLS terminating inside the enclave**, pinned to named providers — the host forwards opaque bytes.
- The ticket-02 verifier runs in-enclave with the ≥2-provider quorum. At N=1 this is the TCB; document it.

### 5. Ops
EC2 Nitro host provisioning (terraform per repo conventions — new ECR repo in ecr.tf per redeploy gotchas), enclave lifecycle scripts (build/run/attach), boot flow: start → generate ephemeral key → `/get_attestation` → operator registers `Enclave` object on-chain.

## Verify (exit criteria)
- Enclave runs on a Nitro EC2; `/get_attestation` returns a document whose PCRs match the reproducible build and whose signature chain verifies **on-chain** via our `enclave.move` registration flow.
- Signer refuses to serve `/sign_requests` until its ephemeral key is registered on-chain.
- RPC egress works only to allow-listed providers; a MITM'd provider (test harness) fails TLS pinning and blocks signing rather than forging a chain view.
- End-to-end: live testnet message signed from inside the enclave, delivered both directions.

**Depends on:** 02 (verifier), 06 (API shape). **Blocks:** 08, 09.
