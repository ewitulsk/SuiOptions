# infra-bridge

Isolated Terraform for the bridge signer's **AWS Nitro enclave host**
(bridge_tickets/07 Phase 5). Stands up a single `c7g.large` Graviton instance
that's Nitro-Enclave-ready, plus its ECR repo, IAM, and security group.

## Why a separate root

Deliberately **not** part of `rust-backend/infra/`:
- Own (local) state — no shared state with the main root, which carries a known
  destructive-drift landmine (its `ecr.tf` `for_each` with `state rm` warnings).
  So `apply` here never needs `-target` gymnastics.
- Different arch/OS (arm64 Graviton + Nitro vs the main root's amd64 Ubuntu).

It **reuses** the main VPC + a public subnet via data-source lookup (by the
`options-vpc` / `options-public-0` tags) — it does not recreate networking.

## What it creates

| Resource | Notes |
|---|---|
| `aws_instance.enclave` | `c7g.large`, AL2023 arm64, **`enclave_options { enabled = true }`**, IMDSv2, gp3 30 GB. `user_data` installs `nitro-cli` + docker, configures the allocator (1 vCPU / 1536 MiB to the enclave), enables SSM. |
| `aws_ecr_repository.enclave` | `options-bridge-signer-enclave`, immutable tags. Push target for `bridge-enclave.yml`. |
| IAM role + instance profile | SSM (no SSH) + ECR pull (scoped to the repo). |
| Security group | Egress all (HTTPS/SSM/ECR/RPC/Seal); ingress none by default (tcp/3000 only if `signer_api_ingress_cidrs` set). |

The host comes up **enclave-ready but not running an enclave** — there's no EIF
until ticket 07 Phase 1. After apply, build the EIF (CI) and run it over SSM.

## Run it

```bash
cd rust-backend/infra-bridge
cp terraform.tfvars.example terraform.tfvars   # tweak if needed
terraform init
terraform plan
terraform apply
```

Then wire CI: set the repo variable **`BRIDGE_ENCLAVE_ECR_REPO`** to the
`ecr_repo_url` output's repo name so `.github/workflows/bridge-enclave.yml`
pushes to it. Reach the host with the `ssm_session_hint` output (SSM, no SSH).

## Caveats (verify on first apply — I couldn't run this)

- **Package names:** `aws-nitro-enclaves-cli{,-devel}` + `docker` via `dnf` on the
  pinned AL2023 release. If a name differs, adjust `templates/user_data.sh.tftpl`.
- **Nitro CLI version affects PCR0** — pin it deliberately once we lock the build.
- **N=1 now.** For N≥3 (ticket 09), refactor the instance/IAM/SG into a
  `bridge_signer_node` module and `for_each` over operators/subnets.
- Local state; add an S3 `backend` block before this is a shared/team resource.
