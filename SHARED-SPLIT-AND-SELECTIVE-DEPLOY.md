# Shared split + selective deploy

A working summary of the refactor that lives on branch
`shared-split-and-selective-deploy` (worktree `~/Dev/options-2`), branched
off `staging` at `85c60fd`.

Two goals, executed together because they're the same problem:

1. **Split the monolithic `shared` crate** into focused sub-crates so
   each service's compile-time dep tree only contains what it actually
   uses. This gives the deployment pipeline a precise way to ask "did
   this PR's changes affect service X?".
2. **Make GH Actions only rebuild + redeploy the services that
   changed.** Today every push to `staging` or `main` rebuilds and
   redeploys all services. After this, an `services/indexer/**` PR only
   touches indexer; a `crates/pricing/**` PR only touches mm-bot.

`cargo check --workspace --all-targets` is clean on the branch. 98
files modified, 17 new files.

---

## Part 1 — Crate split

### Before

```
rust-backend/
├── shared/                       (one big crate, 13 modules)
│   └── src/
│       ├── config_load.rs        (boot)
│       ├── secrets.rs            (boot)
│       ├── logging.rs            (boot)
│       ├── program_spec.rs       (TUI metadata)
│       ├── pricing.rs            (mm-bot math)
│       ├── deployments.rs        (deployments.json loader)
│       ├── sui_client.rs         (chain RPC + signer)
│       ├── quote_signer.rs       (3-scheme signer)
│       ├── ws_client.rs          (WS helpers)
│       ├── protocol_types/       (wire types)
│       ├── pyth/                 (Pyth Hermes client)
│       └── tx/                   (PTB builders)
└── services/* + tools/* + tests/
       └── all depend on `shared = { workspace = true }`
```

Any change to any module in `shared` cascaded into a full rebuild of
every binary that depended on shared — which was every binary.

### After

```
rust-backend/
├── crates/
│   ├── protocol-types/   wire types (asset, ids, events, messages,
│   │                     quote, sides, signing_scheme, coding, errors)
│   │                     + PriceFeedId (lifted from pyth)
│   ├── runtime-config/   config_load, secrets, logging
│   ├── cli-spec/         program_spec + define_program! macro
│   ├── pyth-client/      cache, http, stream, types (minus
│   │                     PriceFeedId), vol
│   ├── sui-tx/           sui_client, tx/*, quote_signer, ws_client
│   ├── pricing/          Black-Scholes
│   └── deployments/      deployments.json loader (split out so
│                         indexer doesn't pull in sui-tx just to load
│                         the package id)
├── services/             each binary's Cargo.toml lists the minimal
├── tools/                set of crates it consumes
└── tests/
```

### Cross-crate dependency graph

```
                  protocol-types        runtime-config        cli-spec
                       ▲                       ▲                 ▲
                       │                       │                 │
        ┌──────────────┼─────────┐             │                 │
        │              │         │             │                 │
   pyth-client     deployments   │             │                 │
        ▲              ▲         │             │                 │
        │              │         │             │                 │
        │              └─────────┴────────┐    │                 │
        │                                 │    │                 │
        │                              sui-tx ─┘                 │
        │                                 ▲                      │
        │           pricing               │                      │
        │              ▲                  │                      │
        │              │                  │                      │
   ┌────┴──────────────┴──────────────────┴──────────────────────┘
   │
   ▼ used by the binaries:
   indexer          ← protocol-types, runtime-config, cli-spec, deployments
   quoting-service  ← protocol-types, runtime-config, cli-spec
   mm-bot           ← protocol-types, runtime-config, cli-spec, pyth-client,
                      pricing, deployments, sui-tx
   option-scheduler ← protocol-types, runtime-config, cli-spec, pyth-client,
                      deployments, sui-tx
```

`shared` is deleted. No facade crate — the cut was atomic.

### Cycle-break decisions

Two internal couplings inside the old `shared/` needed resolution:

1. **`deployments.rs` → `pyth::PriceFeedId`** (`TokenSpec::pyth_feed()`
   returned a `PriceFeedId`). With pyth and deployments in different
   crates, this would be a downstream → downstream edge.
   **Resolution**: lifted `PriceFeedId` (32-byte newtype, ~30 lines)
   into `protocol-types`. Both `pyth-client::types` and `deployments`
   now reference it from there. `pyth-client::types` re-exports it for
   call-site continuity.

2. **`sui_client::Signer::from_secrets` referenced `crate::Secrets`**
   and **`tx/*` referenced `crate::SigningScheme`** and `tx/execute_write`
   referenced `crate::sui_client::Signer`. All of these are now
   single-direction edges since `tx`, `sui_client`, `quote_signer`,
   and `ws_client` all live together in `sui-tx`. Cross-crate refs
   rewrite to `runtime_config::Secrets` and `protocol_types::SigningScheme`.

No true cycle existed — I had flagged one based on a misread grep
during planning. The real graph is a DAG.

### Why `deployments` got its own crate

Originally I planned to keep `deployments` inside `sui-tx`. But indexer
needs `Deployments::load()` to resolve its config's `network` field to a
package id, and indexer doesn't otherwise touch the chain (it reads
checkpoints directly via `sui-data-ingestion-core`). Putting deployments
in `sui-tx` would have made every `tx/*.rs` or `quote_signer.rs` change
mark indexer "affected" — defeating the point of selective deploys.

`deployments` is tiny (one file, ~370 lines) and depends only on
`protocol-types`, `serde`, `anyhow`, and `sui-types::base_types`. Worth
its own crate.

### Import rewrite (sed)

Mechanical rewrite across `services/`, `tools/`, `tests/`:

| Old | New |
|---|---|
| `shared::protocol_types::*` | `protocol_types::*` |
| `shared::config_load`, `shared::logging`, `shared::Secrets` | `runtime_config::*` |
| `shared::define_program!`, `shared::program_spec::*` | `cli_spec::*` |
| `shared::pricing::*` | `pricing::*` |
| `shared::pyth::*` | `pyth_client::*` |
| `shared::sui_client::*`, `shared::tx::*`, `shared::quote_signer::*`, `shared::ws_client` | `sui_tx::*` |
| `sui_tx::deployments::*` (after sed) | `deployments::*` |

One pattern needed a hand fix: `use shared::pyth::{self, ...}` (in
`services/mm-bot/src/main.rs` and `services/option-scheduler/src/spot.rs`)
became `use pyth_client::{self, ...}` after sed, but the rest of those
files used the bare identifier `pyth::http::latest(...)`. Aliased to
`use pyth_client as pyth` / `use pyth_client::{self as pyth, ...}` so
the call sites kept working.

### `logging.rs` crate list update

`runtime-config/src/logging.rs::OUR_CRATES` was a hardcoded list of
workspace crate names that get the `RUST_LOG` level applied (everything
else stays at `warn`). Updated to drop `"shared"` and add the new crate
names + `"option_scheduler"` (which was missing pre-refactor):

```rust
const OUR_CRATES: &[&str] = &[
    "protocol_types", "runtime_config", "cli_spec", "pyth_client",
    "sui_tx", "pricing",
    "indexer", "quoting_service", "mm_bot", "option_scheduler",
    "deployment_manager", "exchange", "writer", "control_panel",
    "integration_tests",
];
```

### `define_program!` macro relocation

The macro was defined in `shared/src/lib.rs` with `#[macro_export]` and
referenced `$crate::program_spec::ProgramSpec`. It now lives in
`crates/cli-spec/src/lib.rs` and references `$crate::ProgramSpec` (the
re-export at the cli-spec crate root). Call sites change from
`shared::define_program! { ... }` to `cli_spec::define_program! { ... }`.
All six callers (every service + the four tools that have a CLI)
verified compiling.

### Files that aren't a direct match

A handful of dependency declarations weren't trimmed beyond the
`shared = …` line (e.g., quoting-service still declares `config` and
`dashmap` even though it now consumes them transitively via
`runtime-config`/`pyth-client`). They're harmless and `cargo check`
accepts them. Worth a follow-up "minimal-dep audit" PR if you want it
tighter.

---

## Part 2 — Deployment pipeline

### `Dockerfile.scheduler`

New, mirrors `Dockerfile.mm-bot` (same multi-stage builder, same
runtime base). One twist in the entrypoint: option-scheduler takes
`--network` as a CLI flag (no env-var fallback in its `Cli`), and the
right value depends on `APP_ENV`. So the entrypoint maps:

```
APP_ENV=dev     → --network testnet
APP_ENV=staging → --network devnet
APP_ENV=prod    → --network mainnet
```

via a shell `case` in the entrypoint string.

### `deployment/bake.hcl`

- Added `option-scheduler` target.
- Split the gha cache scope per target (`scope = "indexer"` etc.) so a
  change to one service's source doesn't invalidate another's cached
  layers. With matrix-driven per-service builds, the global `gha` cache
  was getting cross-contaminated.
- `default` group still lists all four targets — used by `force_all`
  and by `docker buildx bake` invocations from a dev machine.

### `infra/ecr.tf`

Added `option-scheduler` to `local.service_repos`. `terraform apply`
creates the `options/option-scheduler` ECR repo with the same
keep-last-20 lifecycle policy.

### `infra/iam.tf`

Added one statement to the EC2 inline policy:

```hcl
{
  Effect = "Allow"
  Action = ["s3:GetObject"]
  Resource = [
    "${aws_s3_bucket.ssm_output.arn}/deploy-bundles/*",
  ]
},
```

This is what lets the EC2 box download the per-deploy bundle that the
workflow uploads (see SSM step below). Scoped to `deploy-bundles/*` so
the EC2 can't read arbitrary command-output objects.

### Compose files — per-service tags

Replaced single `${IMAGE_TAG}` with `${INDEXER_TAG}`, `${QUOTING_TAG}`,
`${MM_BOT_TAG}`, `${SCHEDULER_TAG}`. `.env` on the EC2 box carries all
four lines (one per service declared in that env's compose file).
`deploy.sh` updates only the tags for services it was asked to roll;
docker compose's variable substitution resolves the others to their
prior values.

option-scheduler service added to `docker-compose.dev.yml` and
`docker-compose.staging.yml`. In `docker-compose.prod.yml` both
`mm-bot` and `option-scheduler` are commented out — same reason: no
mainnet bootstrap path for either, and prod's `config.prod.toml` for
the scheduler is intentionally pairs-less.

### `deployment/ec2/deploy.sh` — selective deploys

New signature:

```
deploy.sh <env> <services-json>
# e.g.  deploy.sh dev '["mm-bot","indexer"]'
```

Behavior:

1. Parses the services list with `jq`.
2. Filters out services not declared in this env's compose file —
   asking for `mm-bot` in prod is logged + skipped, not an error.
3. Snapshots prior tags from `.env` per planned service (for rollback).
4. Calls `render-secrets.sh "$ENV"` (idempotent — renders all per-env
   secrets present in Secrets Manager, skips ones that aren't).
5. Builds the new `.env` atomically: carries prior tags for un-rolled
   services, overlays `IMAGE_TAG` on the planned services.
6. **Pre-roll validation**: every service declared in compose must
   have a tag in `.env`. If any is missing (fresh box, or a service
   newly added to compose), errors out with the canned guidance
   "run a force_all deploy first to seed every service's tag".
7. `docker compose pull <planned>` then `docker compose up -d <planned>`.
8. Health-checks quoting on `:90{1,2,3}2/health` (per-env port) only
   if quoting is in the planned set — the only service with `/health`
   today.
9. On health-check fail, reverts only the planned services' `.env` tag
   lines to the snapshotted values and rolls them back. Other
   services are untouched.

`--remove-orphans` removed from the `up` call — with selective deploys
we'd never want compose to delete a service that wasn't in this
deploy's planned set.

### `deployment/ec2/render-secrets.sh`

Extended with a scheduler block. Expects
`options/<env>/scheduler` in Secrets Manager with shape
`{"sui_key":"suiprivkey1..."}` (the deployer key — AdminCap holder),
renders to `/opt/options/<env>/secrets/scheduler.toml` as:

```toml
[sui]
testnet = "suiprivkey1..."
```

Like the mm-bot section, silently skips when the secret doesn't exist
— that's the supported way to keep scheduler out of an env.

### `services/option-scheduler/config/`

Added `config.dev.toml`, `config.staging.toml`, `config.prod.toml` to
match the pattern other services use (one config per APP_ENV). dev and
staging are identical for now (TBTC/TUSDC and TDEEP/TUSDC pairs). prod
is intentionally pairs-empty — the binary will refuse to start ("no
[[pairs]] configured"), which is the desired behavior since prod
scheduler isn't deployed.

`config.toml` (the local-dev default) stays untouched.

---

## Part 3 — GitHub Actions

Three workflow files under `rust-backend/.github/workflows/`:

```
_deploy.yml           reusable, workflow_call
deploy-lower.yml      push:staging → calls _deploy.yml with envs=["dev","staging"]
deploy-prod.yml       push:main    → calls _deploy.yml with envs=["prod"]
```

### `_deploy.yml` — three jobs

#### 1. `affected`

Fetches full git history, runs `tj-actions/changed-files@v44` against
`github.event.before..HEAD`, pipes the changed-file list into
`deployment/affected.py`. Outputs:

- `services` — JSON array, e.g. `["indexer","mm-bot"]`
- `tag` — short SHA, or the `image_tag` dispatch input if set
- `skip_build` — true when `image_tag` was set (rollback path)

When `force_all=true` OR `image_tag` is set, `services` is the full
list (rollback expects to bring the whole stack to a known-good tag;
if you want one service only, do it manually for now).

#### 2. `build`

Matrix over `services`. Each job runs:

```
docker buildx bake -f deployment/bake.hcl --push <service>
```

`fail-fast: false` so one service's build failure doesn't cancel the
others. Skipped on rollback runs (the image is already in ECR).

#### 3. `deploy`

Matrix over `envs` (`fail-fast: false`, concurrency scoped to
`deploy-${{ matrix.env }}` with `cancel-in-progress: false` — pushes to
the same env queue rather than racing).

Each deploy job:

1. **Bundles** `deploy.sh`, `render-secrets.sh`, and this env's
   `docker-compose.<env>.yml` into a tarball, uploads to
   `s3://$SSM_OUTPUT_BUCKET/deploy-bundles/<tag>-<env>.tgz`. This is
   how a script change in the repo reaches the EC2 box on the next
   deploy — no cloud-init re-run needed.
2. **SSM command** (params file built with `jq` so the JSON-in-shell
   escaping of the services list stays mechanical):
   ```
   set -euo pipefail
   export AWS_REGION=… ECR=… IMAGE_TAG=… DB_HOST=…
   aws s3 cp s3://…/deploy-bundles/<tag>-<env>.tgz /tmp/bundle.tgz
   mkdir -p /opt/options/<env>
   tar xzf /tmp/bundle.tgz -C /opt/options/<env>
   chmod +x /opt/options/<env>/deploy.sh /opt/options/<env>/render-secrets.sh
   cd /opt/options/<env>
   ./deploy.sh <env> '<services-json>'
   ```
3. Waits, polls `aws ssm get-command-invocation`, prints stdout, and
   on non-Success prints stderr + exits non-zero.

### `deploy-lower.yml` / `deploy-prod.yml`

Reduced to ~30-line callers. Same triggers as before (push to `staging`
/ `main` + `workflow_dispatch`). Dispatch inputs:

- `image_tag` (optional) — rollback to this tag.
- `force_all` (boolean, default false) — bypass the affected-files
  filter.

---

## Part 4 — `deployment/affected.py`

Single source of truth for the path → service mapping. Pure Python,
stdlib only (`fnmatch`, `json`, `sys`). Reads a newline-separated
changed-file list from stdin (or argv), writes a JSON array of
affected service names to stdout.

Two pattern sets:

- **`REBUILD_ALL_GLOBS`** — globs that force every service to rebuild.
  Today: `rust-backend/Cargo.lock`, `rust-backend/Cargo.toml`,
  `rust-backend/deployments.json`, `rust-backend/deployment/**`,
  `rust-backend/infra/**`, `.github/workflows/**`.
- **`SERVICE_GLOBS`** — per-service globs covering the service's own
  source tree, its Dockerfile, and each crate it depends on.

The mapping mirrors each binary's `Cargo.toml` dep list — they must
stay in sync. Comment in the file calls this out.

### Smoke tests (run during build)

```
$ echo "rust-backend/services/indexer/src/main.rs" | python3 affected.py
["indexer"]

$ echo "rust-backend/crates/pricing/src/lib.rs" | python3 affected.py
["mm-bot"]

$ echo "rust-backend/crates/protocol-types/src/quote.rs" | python3 affected.py
["indexer","mm-bot","option-scheduler","quoting-service"]

$ echo "rust-backend/crates/pyth-client/src/cache.rs" | python3 affected.py
["mm-bot","option-scheduler"]

$ echo "rust-backend/Cargo.lock" | python3 affected.py
["indexer","mm-bot","option-scheduler","quoting-service"]

$ echo "README.md" | python3 affected.py
[]
```

All pass.

---

## Effective rebuild matrix

| Touched path | indexer | quoting | mm-bot | scheduler |
|---|:-:|:-:|:-:|:-:|
| `services/indexer/**` | ✓ | — | — | — |
| `services/quoting-service/**` | — | ✓ | — | — |
| `services/mm-bot/**` | — | — | ✓ | — |
| `services/option-scheduler/**` | — | — | — | ✓ |
| `crates/pricing/**` | — | — | ✓ | — |
| `crates/pyth-client/**` | — | — | ✓ | ✓ |
| `crates/sui-tx/**` | — | — | ✓ | ✓ |
| `crates/deployments/**` | ✓ | — | ✓ | ✓ |
| `crates/protocol-types/**` | ✓ | ✓ | ✓ | ✓ |
| `crates/runtime-config/**` | ✓ | ✓ | ✓ | ✓ |
| `crates/cli-spec/**` | ✓ | ✓ | ✓ | ✓ |
| `Cargo.lock`, `infra/**`, workflows/** | all four (rebuild_all) |
| docs only / `tests/**` | — | — | — | — |

`tests/**` doesn't appear in any path map, so a tests-only PR
deploys nothing. That's the new floor.

---

## Required post-merge actions

These have to happen once after merging this branch. The workflow is
designed to fail loudly + understandably if any of them is skipped.

### 1. `terraform apply` in `rust-backend/infra/`

Picks up:

- **New ECR repo** `options/option-scheduler` (with the same
  keep-last-20 lifecycle policy as the others).
- **`s3:GetObject` on `deploy-bundles/*`** added to the EC2 inline
  policy. The bundle-download step in the new SSM command needs this.

Both are non-destructive — no resource replacement, no downtime.

### 2. First deploy must use `force_all=true`

The new `deploy.sh` pre-validates that every service declared in
compose has a tag in `.env`. On a box that's never run the new
`deploy.sh`, `.env` has the old single `IMAGE_TAG` and none of the
per-service tags. A normal selective deploy would fail with:

```
ERROR: indexer is declared in docker-compose.dev.yml but no INDEXER_TAG in .env
       run a force_all deploy first to seed every service's tag
```

Run the lower-env workflow manually with `force_all=true` to seed all
four per-service tags simultaneously. After that, selective deploys
work.

### 3. Create `options/<env>/scheduler` in Secrets Manager

`render-secrets.sh` looks for `options/<env>/scheduler` and silently
skips if it doesn't exist (mirrors mm-bot's behavior). Without the
secret, the scheduler container can't read `/run/secrets/scheduler.toml`
and will fail to start with a missing-file error.

Format expected:

```json
{"sui_key": "suiprivkey1...<deployer key for this env>..."}
```

Per env that should run the scheduler — dev and staging today. Prod is
intentionally commented out in `docker-compose.prod.yml`.

### 4. (Maybe) Move workflows to repo root

GitHub Actions only reads workflows from `<repo-root>/.github/workflows/`.
Yours live at `rust-backend/.github/workflows/`. If these have never
fired before, that's why. To enable:

```bash
git mv rust-backend/.github .github
```

I left them where you had them to match your existing layout — flagging
this so you can decide.

---

## Verification done

- `cargo check --workspace --all-targets` clean.
- `affected.py` smoke-tested against representative changed-file
  scenarios; outputs match expectations.
- `git mv` used for every source-file relocation, so `git log --follow`
  on the new paths preserves the pre-refactor history.

## Verification not done

- `cargo test --workspace` — should pass (test code compiles), but
  some integration tests want a running indexer + Postgres + WS server,
  so they're not part of the standard `cargo check` flow.
- End-to-end deploy on a real EC2 — the SSM command is hand-built;
  worth a first run with `force_all=true` to validate the bundle/sync
  path before relying on selective deploys.
- `tj-actions/changed-files@v44` behavior on the first run — the
  workflow needs `fetch-depth: 0` which it has, but the first push
  after merge to staging may pull a large diff. Force-all the first
  one to be safe.

---

## What's intentionally not done

- **`deployment.md` not rewritten.** The doc still describes the
  pre-refactor pipeline. Worth a separate sweep once the new pipeline
  has been observed end-to-end.
- **Per-service rollback via workflow_dispatch.** `image_tag` override
  applies to all services in the affected set (which becomes the full
  set when `image_tag` is set). For "roll mm-bot back to abc123 only",
  do a hand-rolled `aws ssm send-command` for now.
- **Cargo.lock change-impact narrowing.** Today any lockfile change
  forces all four services to rebuild. A future optimization could
  parse the lock diff and only mark services whose actual dep tree
  changed.
- **Dep diet for binaries.** A few binaries (quoting-service,
  option-scheduler) still declare deps in their Cargo.toml that they
  now consume transitively via `runtime-config` / `cli-spec`. Harmless
  but tidier to clean up.
- **No commit.** The branch is on staging's SHA with the diff all in
  the working tree. Commit when you're ready to review.

---

## File-by-file changes

### New files (17)

```
rust-backend/Dockerfile.scheduler
rust-backend/SHARED-SPLIT-AND-SELECTIVE-DEPLOY.md     (this file, OPTIONAL)
rust-backend/.github/workflows/_deploy.yml
rust-backend/crates/cli-spec/Cargo.toml
rust-backend/crates/cli-spec/src/lib.rs
rust-backend/crates/deployments/Cargo.toml
rust-backend/crates/pricing/Cargo.toml
rust-backend/crates/protocol-types/Cargo.toml
rust-backend/crates/protocol-types/src/pyth_id.rs
rust-backend/crates/pyth-client/Cargo.toml
rust-backend/crates/runtime-config/Cargo.toml
rust-backend/crates/runtime-config/src/lib.rs
rust-backend/crates/sui-tx/Cargo.toml
rust-backend/crates/sui-tx/src/lib.rs
rust-backend/deployment/affected.py
rust-backend/services/option-scheduler/config/config.dev.toml
rust-backend/services/option-scheduler/config/config.prod.toml
rust-backend/services/option-scheduler/config/config.staging.toml
```

### Renames (29 — all of `shared/src/*` to `crates/*/src/*`)

All `R` or `RM` in git status — history preserved.

### Modified (in-place)

```
rust-backend/Cargo.toml                       workspace members + deps
rust-backend/Cargo.lock                       regenerated
rust-backend/deployment/bake.hcl              + scheduler target, per-target cache scope
rust-backend/deployment/compose/docker-compose.{dev,staging,prod}.yml
                                              per-service _TAG vars, + scheduler
rust-backend/deployment/ec2/deploy.sh         selective-deploy rewrite
rust-backend/deployment/ec2/render-secrets.sh + scheduler.toml render
rust-backend/infra/ecr.tf                     + option-scheduler ECR repo
rust-backend/infra/iam.tf                     + s3:GetObject for deploy-bundles
rust-backend/.github/workflows/deploy-lower.yml    thin caller of _deploy.yml
rust-backend/.github/workflows/deploy-prod.yml     thin caller of _deploy.yml
rust-backend/services/*/Cargo.toml            per-service dep diet
rust-backend/services/*/src/**/*.rs           shared:: → new-crate:: imports
rust-backend/tools/*/Cargo.toml               per-tool dep diet
rust-backend/tools/*/src/**/*.rs              shared:: → new-crate:: imports
rust-backend/tests/Cargo.toml                 shared → protocol-types
rust-backend/tests/src/**/*.rs                shared:: → protocol_types:: imports
```
