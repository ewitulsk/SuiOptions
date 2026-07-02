# 07 — Nautilus enclave: run the signer inside AWS Nitro

**Spec:** bridge-spec.md §5.1–§5.4 · **Milestone:** M3 entry · **Status:** IN PROGRESS
**Landed:** Phase 2 (arm64 CI build/deploy workflow + PCR0 drift gate) and Phase 3 (on-chain enclave registry — `sui-bridge-contracts/enclave/`, 7 Move tests). **Remaining (needs Nitro hardware):** Phase 1 (nautilus port + in-enclave TLS egress), Phase 4 (in-enclave chain view), Phases 5–6 (terraform + lifecycle).
**Why:** today `bridge-signer-service` is a plain process with curve seeds in config. The spec's trust model requires it to run inside an attested AWS Nitro enclave so that (a) only approved code, registered on-chain, can produce signatures, and (b) the enclave's *chain view* — the §5.4 "was this committed at finality" check — can't be forged by the untrusted host. This ticket is the enclave migration only; Seal-based key provisioning is ticket 08 (this one can boot with a config-injected seed for staging so the two are independently testable).

**Hard prerequisite:** a Nitro-Enclave-capable EC2 instance with `nitro-cli`. The vCPU floor is processor-dependent — **Intel/AMD need ≥4 vCPU** (whole hyperthread pairs are dedicated to the enclave and ≥2 vCPU must remain for the parent → smallest is `*.xlarge`), but **Graviton needs only ≥2 vCPU** (no SMT → 1 parent + 1 enclave → smallest is `*.large`). Bare-metal, T-family burstable (t3/t4g), and single-core instances are excluded regardless. Our signer workload is light (axum + rustls + the §5.4 verifier), so **default to a `c7g.large` (2 vCPU / 4 GB Graviton)** — ample and cheaper than an Intel xlarge. Caveats: the EIF must be built for **aarch64** and **PCRs are arch-specific** (don't mix arches across the signer set); revisit sizing only if in-enclave crypto (ticket 09) turns CPU-heavy. None of this is doable in the dev sandbox.

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

### Phase 2 — Reproducible build & PCR measurement (GitHub Actions, arm64)
1. Build the EIF via the nautilus reproducible Dockerfile: pinned Rust toolchain, base image pinned **by digest**, `cargo build --locked --release`, `SOURCE_DATE_EPOCH` set, no build timestamps. Goal: **bit-identical EIF → identical PCR0** across machines/runs.
2. `nitro-cli build-enclave --docker-uri <img> --output-file signer.eif` → emits PCR0/1/2. Commit the expected PCR0 to the repo.
3. **CI drift gate:** rebuild on a clean runner and assert PCR0 == committed. Drift ⇒ the on-chain `EnclaveConfig` would reject the real enclave — catch it here, not at deploy.

**Runner architecture — build the EIF NATIVELY on arm64, never x86+QEMU** (emulation is slow and breaks PCR determinism). GitHub *does* have arm64 hosted runners (the earlier "x86 only" belief is outdated), but for this **private** repo they require Team/Enterprise ("larger runners"). Two supported paths:
- **Team/Enterprise plan:** `runs-on: ubuntu-24.04-arm` (or a labeled arm64 larger runner). Native, clean.
- **Otherwise (default assumption):** a **self-hosted arm64 runner on a small Graviton box** (e.g. a `t4g`/`c7g` build instance — burstable is fine for *building*, it's only excluded for *running* enclaves). This box also reliably runs `nitro-cli build-enclave`.

**Verify at build time:** whether `nitro-cli build-enclave` runs on a *hosted* runner without the `nitro_enclaves` kernel module. The build/measurement step generally does not need Nitro hardware, but if a hosted runner can't run it, that forces the self-hosted-Graviton path — so plan for the self-hosted runner as the safe default.

**Pipeline outline** (`.github/workflows/bridge-enclave.yml`):
```
jobs:
  build-eif:
    runs-on: [self-hosted, linux, arm64]     # or ubuntu-24.04-arm on Team/Enterprise
    steps:
      - checkout
      - build reproducible docker image (pinned digest, --locked, SOURCE_DATE_EPOCH)
      - nitro-cli build-enclave --docker-uri $IMG --output-file signer.eif
      - PCR0=$(jq -r .Measurements.PCR0 build-output.json)
      - test "$PCR0" = "$(cat expected_pcr0.txt)"   # drift gate — fail on mismatch
      - on tag: docker push $IMG to ECR (pinned by digest)   # host rebuilds the SAME EIF
      - upload signer.eif + measurements as artifacts / into the EnclaveConfig manifest
```
Ship the **image pinned by digest** (host `build-enclave`s the same digest → same EIF → same PCRs), or ship the EIF artifact directly. The measurements feed the governance step that sets `EnclaveConfig` PCRs on-chain (Phase 3).

**Note on the existing arm backend workflows:** the other services already target arm (cross-compiled or on arm runners), but the enclave EIF is stricter — it needs a *native* arm64 build for reproducibility, so it can't ride an x86-runner + cross-compile path even if the plain services do.

### Phase 3 — On-chain enclave registry (`enclave.move`) ✅ LANDED
Implemented in `sui-bridge-contracts/enclave/` (package `bridge_enclave`), adapted from nautilus `enclave.move` (Apache-2.0); attestation verification is native in the Sui framework (`sui::nitro_attestation`, confirmed present in the 1.71 toolchain).
1. `enclave::enclave` — `EnclaveConfig<T>` (shared, governance-`Cap`-gated PCRs + version), `register_enclave(config, NitroAttestationDocument)` → per-node `Enclave<T>` object holding the attested ephemeral pubkey; `verify_signature` for enclave-signed intents. ✅
2. `enclave::signer` — the `BRIDGE_SIGNER` witness + `init` minting the governance `Cap`. ✅
3. **Bridge additions over upstream:** `update_enclave_pk` re-registers a fresh ephemeral key into the SAME `Enclave` object (stable object id, so the ticket-08 Seal binding survives a restart — §6.4); owner-gated; register/update **events** for ticket-10 alerting; `destroy_old_enclave` version-gated retirement. ✅
4. **Per-node operator credential:** `Enclave.owner` (the registrant) gates `update_enclave_pk`/`destroy_enclave_by_owner`; custody decision → ticket 10.
5. **Tested (7 Move tests):** PCR/version + cap gating, owner gating, stale-version retirement, bad-signature rejection, witness-cap minting. **NOT unit-testable here:** the `register_enclave`/`update_enclave_pk` attestation path needs a real `NitroAttestationDocument` (framework-native, real enclave + hardware) — integration-test in Phase 1/6. Test-only `deploy_for_testing` exercises the registry logic without a doc.

### Phase 4 — Chain view inside the boundary (§5.4)
1. The ticket-02 `RpcVerifier` runs *in-enclave* with the ≥2-provider quorum, over the in-enclave-TLS transport from Phase 1. Carry forward the two live-run fixes: **bounded `eth_getLogs` lookback** (public RPCs cap the range) and tolerance for public-RPC 500s (retry/failover across the quorum providers).
2. Provider certs pinned in the enclave image (part of PCR measurement) so a swapped provider changes the PCRs.
3. Document the TCB honestly: at N=1 the enclave + its (pinned, in-enclave-verified) chain view is the trust root; the k-of-n guarantee arrives at ticket 09.

### Phase 5 — Infra: Terraform (a new, isolated root)

**Use a new Terraform root — do NOT extend `rust-backend/infra/`.** That root is flat, amd64/Ubuntu, local-state, and carries a known destructive-drift landmine (its `ecr.tf` `aws_ecr_repository.svc` for_each has `terraform state rm` warnings; a blanket `apply` there destroys the derived-metric-worker ECR repo + edits IAM — see the repo's terraform-drift notes). Isolating the enclave infra in its own root with its own state means we never have to `-target` around that, and the arch/OS differ anyway (arm64 + Nitro vs amd64).

**New root: `rust-backend/infra-bridge/`** (own `versions.tf` with a **separate state backend key**, not shared with `infra/`). Read the existing network via data sources (or a `terraform_remote_state` data source against the main root's outputs) — reuse the VPC/subnet, don't recreate.

Resources:
1. **`aws_ecr_repository "bridge_signer_enclave"`** — a standalone repo *in this root* (not the shared `svc` for_each map), which sidesteps the drift landmine entirely. (Missing repo → 403 on push, per the redeploy gotchas.)
2. **`aws_instance "bridge_signer"`**:
   - `instance_type = "c7g.large"`, **`enclave_options { enabled = true }`**.
   - `ami` = a **pinned Amazon Linux 2023 (or Ubuntu) arm64** AMI. Pin it, don't `most_recent` (matches the existing convention so a new release doesn't force-replace the host) — and it must be **arm64**, not the amd64 AMI the main root uses.
   - `iam_instance_profile`, `vpc_security_group_ids`, `subnet_id` (data), `root_block_device` gp3 30+ GB.
   - `user_data` (cloud-init, mirror the `infra/templates/` pattern): install `aws-nitro-enclaves-cli` + `-devel` + docker, add the user to the `ne`+`docker` groups, template **`/etc/nitro_enclaves/allocator.yaml`** (`cpu_count: 1`, `memory_mib: 1536`), `systemctl enable --now nitro-enclaves-allocator docker`, pull the image from ECR, `nitro-cli run-enclave`, and start the parent-side vsock forwarder + admin proxy.
3. **`aws_iam_role` + instance profile** (least-priv): ECR pull, `AmazonSSMManagedInstanceCore` (managed via SSM — **no SSH**, per repo ops conventions), CloudWatch Logs, and KMS decrypt only if the Seal/secrets path needs it.
4. **`aws_security_group`**: egress 443 to the RPC providers, Seal servers, ECR, SSM, and CloudWatch endpoints; ingress **tcp/3000 (signer public API) from the relayer's SG/CIDR only**. Admin **3001 stays host-local** (no SG ingress). No inbound 22.

**Allocator math on c7g.large** (2 vCPU / 4 GB): 1 vCPU + ~1.5 GB to the enclave, leaving 1 vCPU + ~2.5 GB for the parent + vsock proxy. Comfortable for the signer workload.

**N=1 now; module-ize for N≥3 later.** Write it as a small module (`bridge_signer_node`) even though we instantiate it once, so ticket 09's N=3 is `for_each` over three operator/subnet inputs.
`outputs.tf`: instance id, private IP, ECR repo URL, the SG id (for the relayer's egress rule).

### Phase 6 — Lifecycle & boot
1. Lifecycle scripts: `build` (EIF + PCRs, in CI per Phase 2), `run` (`nitro-cli run-enclave`), the parent-side vsock forwarder + admin proxy, `attach-console` for debug builds only.
2. Boot flow: enclave starts → generates ephemeral Ed25519 key in-memory → `GET /get_attestation` returns the doc with that pubkey → operator calls `register_enclave` on-chain with their cap → signer flips to "ready".

## Exit criteria
- Enclave runs on a Nitro EC2; `/get_attestation` returns a doc whose PCRs match the reproducible build and whose signature chain **verifies on-chain** via `register_enclave`.
- The signer **refuses `/sign_requests` until its ephemeral key is registered on-chain** (gate on the `Enclave` object existing).
- RPC egress reaches only allow-listed providers; a MITM'd provider (test harness presenting a wrong cert) fails in-enclave TLS pinning and **blocks signing** rather than yielding a forged chain view.
- End-to-end: a live testnet message is signed from inside the enclave and delivered both directions (re-run the ticket-04/round-trip flow with the enclave as signer).

## Effort & sequencing
Largest single ticket in the M3 track. Rough phases: P1 (port + egress rework) ~1–2 wk — the in-enclave TLS transport is the crux; P2 (reproducible build + arm64 CI) ~few days; P3 (enclave.move) ~1 wk; P4 ~few days; P5 (terraform, isolated root) ~1 wk; P6 (lifecycle/boot) ~few days. Do P1–P2 before touching Move; P3 and P5 can proceed in parallel.

**Depends on:** 02 (verifier runs in-enclave), 06 (async API is the enclave surface). **Blocks:** 08, 09.
