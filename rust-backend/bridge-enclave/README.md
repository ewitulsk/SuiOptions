# bridge-enclave

The AWS Nitro Enclave packaging for `bridge-signer-service` (bridge-spec.md §5,
bridge_tickets/07). This directory holds the **enclave image build** and is
consumed by the `.github/workflows/bridge-enclave.yml` CI, which builds the
Enclave Image File (EIF) on a **free public-repo arm64 runner** (`ubuntu-24.04-arm`)
and measures its PCRs.

> **Status: scaffolding (ticket 07 in progress).** Today the Dockerfile packages
> the plain `bridge-signer-service`. The Nautilus integration — the vsock
> listener, `/get_attestation`, in-enclave TLS termination for the §5.4 chain
> view, and the Seal 2-step key load (ticket 08) — is **Phase 1–4 of ticket 07
> and not yet done**. The CI + build shape is stood up first so the rest lands
> against a working pipeline.

## Why the EIF measurement matters

The enclave's `PCR0` is the on-chain trust anchor: the Move `EnclaveConfig`
(ticket 07 Phase 3) will say "only an enclave measuring `PCR0 = X` is our
signer." So the build must be **reproducible** — the same source must yield the
same EIF → the same `PCR0` — and CI's job is to be the independent witness that
reproduces `PCR0` from a clean checkout. `expected_pcr0.txt` pins the approved
value; the workflow fails on drift.

## Files

| File | Purpose |
|------|---------|
| `Dockerfile` | Reproducible build of the enclave app image (context = the `rust-backend` workspace). |
| `.dockerignore` | Keep the build context lean + deterministic. |
| `expected_pcr0.txt` | The approved `PCR0`; the CI drift-gate compares against it (`PENDING` = first-run capture mode, no gate). |

## Known TODOs before this is real (ticket 07)

- **Pin the base image by digest** and the Rust toolchain, set `SOURCE_DATE_EPOCH`,
  strip build timestamps — the current Dockerfile is a functional scaffold, not
  yet bit-reproducible.
- **Pin the exact `nitro-cli` version** in the workflow — `PCR0` depends on the
  nitro-cli / bundled-kernel version, not just the app image.
- **Nautilus wrapper:** replace the entrypoint with the nautilus-server that runs
  the signer inside the enclave over vsock, exposes real attestation, and routes
  all RPC egress through in-enclave TLS (ticket 07 §5.4).
- **Smoke-test to confirm** `nitro-cli build-enclave` runs on the hosted arm64
  runner (no Nitro hardware). If it can't, the workflow's build-enclave step is
  the fallback boundary — move it to a self-hosted Graviton runner (restricted to
  non-fork triggers) or the enclave host. See ticket 07 Phase 2.

## Runtime (later, on the c7g.large host — ticket 07 Phase 5/6)

The host does NOT rebuild the EIF; it runs the exact measured artifact:
`nitro-cli run-enclave --eif-path signer.eif --cpu-count 1 --memory 1536`.
