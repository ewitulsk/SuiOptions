# Bridge enclave — session context (2026-07-03)

Working state of ticket 07 (`bridge_tickets/07-nautilus-enclave.md`, SO-275) after the
2026-07-03 session: the Nitro host is live, the CI build/deploy pipeline works end-to-end,
and the stock-nautilus validation (Phase 1 step 1) passed — including on-chain attestation
verification against the Phase 3 registry. This doc records every artifact, credential
surface, and gotcha so work can resume cold.

**Phase status:** P2 (CI) ✅ · P3 (registry) ✅ · P5 (terraform) ✅ APPLIED · P1 step 1
(stock validation) ✅ → **next is the real P1 work** (vendor + library-fy + vsock/TLS
egress rework), then P4, P6.

---

## 1. Infrastructure (terraform applied — the host is LIVE)

`rust-backend/infra-bridge/` applied 2026-07-03 with zero impact on existing infra
(plan verified additive-only; `options-prod-host` health-checked before/during/after).

| Resource | Value |
|---|---|
| EC2 instance | `i-0ad5124712c538893` — c7g.large (Graviton, 2 vCPU / 4 GB), AL2023 arm64, `enclave_options` enabled, IMDSv2 |
| IPs | private `10.40.1.142`, public `54.144.200.244` (us-east-1, shared `options-vpc` / `options-public-0`) |
| Access | **SSM only, no SSH**: `aws ssm start-session --target i-0ad5124712c538893 --region us-east-1` |
| ECR repo | `502186568577.dkr.ecr.us-east-1.amazonaws.com/options-bridge-signer-enclave` (immutable tags) |
| Host IAM | role/profile `options-bridge-enclave` (SSM core + scoped ECR pull) |
| CI IAM | role `options-bridge-gh-deploy` (see §2) |
| Security group | `sg-091a8455c990ff511` — egress all; ingress tcp/3000 only via `signer_api_ingress_cidrs` (currently empty) |
| Allocator | 1 vCPU / 1536 MiB (`/etc/nitro_enclaves/allocator.yaml`), nitro-cli 1.4.4 + docker verified active |

**Caveats**
- Terraform state is **LOCAL to Evan's machine** (gitignored). No S3 backend yet — no
  other machine can manage these resources. Add a `backend "s3"` block when this stops
  being a one-person root.
- The root is deliberately isolated from `rust-backend/infra/` (which has a known
  destructive-drift landmine). Blanket `terraform apply` in `infra-bridge/` is safe;
  it reads the VPC/subnet via data lookups only.
- `ignore_changes = [ami]` pins the host against AL2023 AMI-release replacement.

## 2. CI pipeline (`.github/workflows/bridge-enclave.yml`) — working

First fully successful deploy run: [28686106945](https://github.com/ewitulsk/SuiOptions/actions/runs/28686106945)
(dispatch with `deploy=true`, env staging). Pushed image
`options-bridge-signer-enclave:8f8a6d8f13f2f36eed154af79cc3eaffc47ea192`
(digest `sha256:f61659dcde586e5f7260b151dd52ca505a4b8571e5fa948b5f4198c8efb2ab28`).

Run it: `gh workflow run bridge-enclave.yml --ref <branch> -f deploy=true -f environment=staging`.
Watch out: a push to enclave paths auto-triggers a build-only run in the same
concurrency group — cancel it before dispatching or the dispatch queues behind it.

**Four fixes were needed (all committed on `ewitulsk/sui-bridge`):**
| Commit | Fix |
|---|---|
| `c8f729d` | `make vsock-proxy` before `make install` — install-tools installs BOTH binaries; only nitro-cli was being built |
| `36853cd` | source nitro-cli's `env.sh` in every step that runs it — from-source install lands in a local prefix (`./build/install`), not `/usr/bin` |
| `f71aa1f` | pre-create `/var/log/nitro_enclaves` — rpm/deb installs create it, from-source doesn't (nitro-cli dies E19) |
| `8f8a6d8` | assume dedicated `BRIDGE_DEPLOY_ROLE_ARN` — the shared `DEPLOY_ROLE_ARN`'s OIDC trust only covers `refs/heads/{staging,main}` and its ECR allowlist lacks the bridge repo |

**Repo variables (set):**
- `BRIDGE_ENCLAVE_ECR_REPO` = `options-bridge-signer-enclave`
- `BRIDGE_DEPLOY_ROLE_ARN` = `arn:aws:iam::502186568577:role/options-bridge-gh-deploy`
  — defined in `infra-bridge/iam_ci.tf`; OIDC trust covers `ewitulsk/sui-bridge`,
  `staging`, `main` refs; carries the scoped ECR-push policy. The shared
  `options-gh-actions-deploy` role was returned to its exact pre-session state.

**Answered smoke-test question (ticket P2):** `nitro-cli build-enclave` DOES work on the
free hosted `ubuntu-24.04-arm` runner — no Nitro hardware or self-hosted Graviton runner
needed for build/measure.

**PCR0 drift gate is deliberately still `PENDING`** (`rust-backend/bridge-enclave/expected_pcr0.txt`):
- the scaffold Dockerfile `COPY . .`s the whole `rust-backend/` workspace, so ANY
  workspace change shifts PCR0 (two CI runs → two different PCR0s, as expected);
- it self-documents as not-yet-bit-reproducible (base images by tag, no
  `SOURCE_DATE_EPOCH`);
- `expected_pcr0.txt` sits inside the docker context → committing a value is circular.

Arm the gate as part of the P1/P2 reproducibility work, not before.

## 3. Stock-nautilus validation (Phase 1 step 1) — PASSED

Ran upstream `MystenLabs/nautilus` @ **`af7535b9d314f034fa7f9f1f208264540d54cde1`**
(pin this in `FORK_DELTA.md` when vendoring) end-to-end on the box:

1. **Build (on-box):** upstream's reproducible Containerfile/Makefile is **amd64-ONLY**
   (x86_64 StageX digests, x86 `bzImage`, hardcoded `--platform linux/amd64`), so
   validation used a plain aarch64 Dockerfile (rust:1.90-slim → bookworm-slim, stock
   `nautilus-server` weather-example + its `run.sh`) + `nitro-cli build-enclave` on the
   host. EIF measurements: PCR0 `25bdd759b0906c3703d0e0dd3a907a6cfc4330a9e20ee025bf13cce7f76ff7eaae691bb8717304b4c968c64b25320485`.
2. **Boot:** `nitro-cli run-enclave --cpu-count 1 --memory 1024` → enclave up (CID 16).
   Secrets handshake over vsock 7777 (`{"API_KEY":"dummy-validation-key"}` via socat, as
   upstream `expose_enclave.sh` does), parent-side `socat TCP4-LISTEN:3000 ↔ VSOCK`
   forward, `GET /` → `Pong!`, `/get_attestation` → 4492-byte COSE_Sign1 doc.
3. **On-chain (testnet, deployer `0xab8d…4865`):** published the Phase 3 package and
   verified the real attestation path that Move unit tests couldn't cover:

| Object | ID |
|---|---|
| Package `bridge_enclave` | `0xeda4ddd012c724e1fdcf8c69abdf3d365a6b52448846ccf8098d011e037cc466` |
| `Cap<BRIDGE_SIGNER>` | `0x5c7fae89626c96ad3ffcac5fbea823461196189bef03c2fb6df2ecc111dfd828` |
| `UpgradeCap` | `0xf4fc772617e4d61850f194ce113daeb830cddb7043110b7d8ddfadd4773902e8` |
| `EnclaveConfig` (validation PCRs) | `0x1b00efcab7113f978fb50a12564778df8fe68c974559e69780ead21722e76ec7` |
| Registered `Enclave` (shared) | `0xeb33c0a8f532f29ca91bde92c69bb42291ee892ea1f08dc690db19573b5911ae` |
| `register_enclave` tx | `HnS4TQz1bHutYB3XCFjALHRvG6HdkX3MMZctcVZ9gYuM` |

The chain verified the doc's signature chain to the AWS Nitro root CA, matched PCRs
against the config, stored the enclave's boot-generated ephemeral pubkey, and emitted
`EnclaveRegistered`. Registration PTB shape (from upstream `register_enclave.sh`):

```
sui client ptb \
  --assign v "vector[<attestation bytes as u8 array>]" \
  --move-call "0x2::nitro_attestation::load_nitro_attestation" v @0x6 \
  --assign doc \
  --move-call "$PKG::enclave::register_enclave<$PKG::signer::BRIDGE_SIGNER>" @$CONFIG doc
```

**Box state as left:** validation enclave may still be running (dummy key, nothing
sensitive) — `nitro-cli terminate-enclave --all` via SSM to kill. Upstream clone at
`/root/nautilus`, EIF at `/root/nautilus-validation.eif`, measurements at
`/var/tmp/nautilus-measurements.json`, attestation at `/var/tmp/attestation.json`.
The validation `EnclaveConfig`/`Enclave` objects are throwaway — a real config (fresh
PCRs, real name) supersedes them when the signer EIF exists.

**Operational gotchas hit:**
- `nitro-cli build-enclave` needs `NITRO_CLI_ARTIFACTS` (or `HOME`) set → E51 otherwise
  (SSM RunShellScript sessions have neither).
- SSM `get-command-invocation` truncates output (~24KB) — pull large files (e.g. the
  9KB attestation hex) in chunks.
- SSM shell mangles multi-line scripts passed as parameters — ship them base64-encoded.
- The box has 1 usable parent vCPU; on-box cargo builds are slow (~30 min for the
  validation image). Fine for validation; real builds belong in CI.

## 4. What's next (remaining ticket 07 work)

1. **Phase 1 (the crux, ~1–2 wk):**
   - Vendor the pinned nautilus subtree into `rust-backend/bridge-enclave/`
     (`src/nautilus-server`, vsock forwarder, `allowed_endpoints.yaml`) + `FORK_DELTA.md`
     recording `af7535b9…` and deltas. Own `Cargo.lock`, NOT a main-workspace member.
   - Library-fy `bridge-signer-service` (expose router + `AppState` as a lib crate);
     nautilus-server app pulls it by path dep.
   - Egress rework: route `EvmProbe`/`SuiProbe`/`SuiClientBuilder` HTTPS through
     vsock→parent-forwarder with **rustls terminating in-enclave** (pinned provider certs).
2. **Phase 2 completion:** arm64 reproducible build (upstream StageX pipeline is
   amd64-only — port it or pin our Dockerfile by digest + `SOURCE_DATE_EPOCH`), then
   commit the real PCR0 and arm the drift gate.
3. **Phase 4:** ticket-02 `RpcVerifier` in-enclave over the vsock/TLS transport.
4. **Phase 6:** lifecycle scripts + boot flow (enclave boots → attest → operator
   registers on-chain → signer flips ready; signer refuses `/sign_requests` until
   its key is registered).
5. Module-ize the terraform for N=3 (ticket 09) and add the S3 state backend.

## 5. Session artifact inventory

- **Committed this session:** CI fixes + OIDC role (`c8f729d`, `36853cd`, `f71aa1f`,
  `8f8a6d8`), `enclave/Move.lock` + `Published.toml` (testnet publish record), this doc.
- **AWS (all in `infra-bridge` terraform state except noted):** instance, SG, host
  role/profile, ECR repo, CI role `options-bridge-gh-deploy` + its inline policy.
  Nothing outside this list was modified; `options-gh-actions-deploy` briefly carried a
  scoped bridge-ECR policy mid-session and was restored to its original state.
- **Testnet:** the five objects in §3 (throwaway validation config/enclave; package +
  caps are real).
- **On the box (ephemeral):** `/root/nautilus`, `/root/nautilus-validation.eif`,
  `/var/tmp/{nautilus-*,attestation.json,run.sh,setup.sh}`.
