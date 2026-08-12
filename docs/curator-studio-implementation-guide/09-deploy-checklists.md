# 09 — Deploy checklists & ops wiring

The house new-service checklist, instantiated for the five new services. Work through one column at a time; `test_affected.py` and a `force_all` staging deploy are the verification.

## 1. Service matrix

| Item | bot-gateway | key-service | provisioner | test-runner | agent-service |
|---|---|---|---|---|---|
| Port(s) | 9019 | 9022 pub / 9023 int | 9024 | 9021 | 9025 |
| nginx-routed | ✅ (public, Fly bots + dashboard) | ❌ | ✅ (JWT routes) | ✅ | ✅ |
| DB (`<name>_<env>`) | bot_gateway | key_service | provisioner | test_runner | agent_service |
| Secrets TOML contents | key-service caller token | caller tokens | `[fly] api_token`, `[sui]` deployer key, key-service token | — (DB only) | `[openrouter] api_key` |
| Signs Sui txs | via key-service | ✖ (signs FOR others) | via key-service + own deployer key (gas) | ❌ | ❌ |
| prod compose | ✅ | ✅ | ✅ | ✅ | ✅ |
| Health-check budget | default 30 | default | 150 (first boot does chain reads) | default | default |

## 2. The 14-step checklist (per service)

1. `rust-backend/services/<name>/` — crate with `[lib]` + `[[bin]]`, `config/config.{toml,staging,prod}.toml`, `secrets.example.toml` where applicable.
2. `rust-backend/Cargo.toml` `members` += the service (and any new shared crate under `[workspace.dependencies]`).
3. `rust-backend/Dockerfile.<name>` — copy `Dockerfile.oracle-service` shape; **`libpq5` in the runtime stage** (all five use diesel); no `--platform=$BUILDPLATFORM`; `--config /app/config/config.${APP_ENV}.toml`; none of the five takes `--network` on the CLI (network lives in config), so no `APP_ENV→NET` case needed — but every `config.prod.toml` says `network = "testnet"` (prod IS a testnet deployment).
4. `rust-backend/deployment/bake.hcl` — target + `group "default"`; cache scope = target name.
5. `rust-backend/deployment/affected.py` — `ALL_SERVICES` + `SERVICE_GLOBS` (service dir + Dockerfile only; crate coverage is derived); run `rust-backend/deployment/test_affected.py`.
6. `rust-backend/deployment/ec2/deploy.sh` — `ALL_SERVICES`, `tag_var_for`, `compose_name_for`, `health_path_for` (+`health_attempts_for` 150 for provisioner).
7. `.github/workflows/_deploy.yml:162` force_all array; `start-service.yml`/`stop-service.yml` choice lists.
8. `docker-compose.{staging,prod}.yml` — service block (image `${ECR}/options/<name>:${<NAME>_TAG}`, `APP_ENV`, `RUST_LOG: info,<name_underscored>=debug`, peer URLs env, secrets volume ro, `depends_on: [token-info]` + peers, `restart: unless-stopped`, `networks: [net]`, **no ports**); nginx `depends_on` for the routed four.
9. `rust-backend/deployment/nginx/nginx.{staging,prod}.conf` — location blocks (`^/<env>/bot-gateway(?:/(?<tail>.*))?$` pattern) for the routed four. **Exact-path discipline** where a service exposes WS (`/v1/ws` — copy the oracle-service WS location precedent).
10. `rust-backend/infra/ecr.tf` `service_repos` += all five.
11. `rust-backend/infra/secrets.tf` — secret + `REPLACE_ME` placeholder version per service per env (lifecycle ignore_changes). **Fill real values before first deploy** — a declared-in-compose service with an absent/placeholder secret crash-loops and rolls back the whole set. (Known gotcha: the placeholder version can clobber a hand-filled secret on first `terraform apply` — restore from AWSPREVIOUS if hit.)
12. `rust-backend/deployment/ec2/render-secrets.sh` — a render block per service (umask 077; hard-fail on missing keys for compose-declared services with a `WARNING`).
13. `rust-backend/deployment/monitoring/` — gatus endpoint per env (`http://options-<env>-<name>-1:<port>/health`, `[BODY] == ok`), prometheus targets `["<name>:<port>"]`; **sync to hosts via the sync-monitoring workflow** (these files feed cloud-init — do not bounce the EC2 hosts by editing user_data paths).
14. DB: `wipe-provision-db.sh` case arm (`bot-gateway` → `bot_gateway` etc.) + `wipe-provision-db.yml` choice list. **Prod DBs are hand-provisioned** — run the provisioning for prod explicitly before first prod deploy.

First deploy of any new service must be **`force_all`** to seed `<NAME>_TAG` in `/opt/options/<env>/.env` (deploy.sh exits 2 otherwise).

## 3. New infra beyond the checklist

- **KMS**: `aws_kms_key` + alias per env for key-service (03 §1); instance-role grants.
- **Fly**: org(s) `curator-studio-staging` / `-prod` + sandbox org (P4); org tokens minted manually (`fly tokens create org`), stored in `options/<env>/provisioner`; the `cs-<env>-runtime` registry app created once by hand.
- **CI**: `python-sdk.yml` workflow (ruff/mypy/pytest); bot-runtime image build + push to `registry.fly.io` on tag (authed with the org token as a GH secret).
- **Frontend**: new Vercel project rooted at `studio-dashboard/` (exchange-dashboard precedent); env vars for the new service URLs set via the Vercel REST API (the CLI env-add-preview path is buggy).

## 4. Contracts deploy (P0) — deltas to the redeploy runbook

The 2026-08-11 redeploy runbook applies, plus:
- deployment-manager publishes `bounded_curator` + `guarded_exchange_adapter` (01 §6); activation allowlists the guarded adapter witness.
- `deployments.json` gains `boundedCurator` / `guardedExchangeAdapter` package records → token-info serves them → gateway/provisioner resolve at runtime.
- **Studio vaults do not survive a contract redeploy** (nothing on staging does): the provisioner needs a `redeploy-reconcile` admin task that marks all `vaults`/`bots` rows STALE and tears down orphaned Fly machines. Run it as part of the redeploy ceremony (add to the runbook).
- Stop-services list for redeploys grows: bot-gateway + provisioner must be stopped alongside the other wallet-sharing services (SO-299 lesson) because the deployer wallet funds gas.

## 5. Alert-id registry additions

Append to `.claude/tx-alerting.md`:

```
tx-failed-bot-gateway-watermark      gateway cancel_up_to submission failed
tx-failed-bot-gateway-guarded-order  guarded DeepBook/PTB submission failed
tx-failed-provisioner-deploy         deploy state-machine on-chain step failed
tx-failed-provisioner-gas-topup      curator wallet gas top-up failed
key-service-sign-refused             PTB failed template match (possible attack)
key-service-kms-error                KMS decrypt/generate failure
bot-heartbeat-missed                 RUNNING bot silent > 3× interval
bot-restart-loop                     ≥3 machine restarts/hour → bot PAUSED
provisioner-fly-api-error            Machines API failure after retries
test-runner-job-failed               sim job failed post-retry
test-runner-data-gap                 standard window missing candles
agent-service-openrouter-error       upstream LLM failures (sustained)
```

Grafana needs **no** per-id change (`alert-id-errors` groups dynamically); gatus/prometheus entries come from step 13.

## 6. Gas-station templates (frontend PTBs)

New sponsored user flows → `protocol_templates()` additions (04 §6): `bounded_curator:unwrap`, `bounded_curator:rotate_curator`, `bounded_curator:set_policy`. Depositor flows reuse existing `vault:*` templates. Follow the in-file 7-step recipe including unit tests and the frontend-builder cross-reference comment.

## 7. Rollout order (P1 first staging deploy)

1. P0 contracts published + activation (04 §4 deltas) — verify drills (01 §7).
2. terraform: ECR repos, secrets (+fill), KMS → apply from the canonical `options-2` worktree (never `~/Dev/options`), plan-before-apply.
3. DBs provisioned (staging).
4. `force_all` staging deploy with all five services in compose.
5. Fly org + runtime registry app + first `bot-runtime` image push.
6. Dogfood drill (spec §19 P1 gate): hand-written spec → provisioner deploy → Fly bot quotes on a guarded staging vault; kill drill; revoke drill.
