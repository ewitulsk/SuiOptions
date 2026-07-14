# Infra & deployment changes for the Solana services

Template: exactly how solana-indexer was wired (deployment.md §11 is stale).
Per new service, all 15 touchpoints below; this guide lists the concrete
values. **The user runs terraform and workflows** — implementation edits the
files; the migration doc carries the run-instructions.

New services (12): solana-token-info, solana-auth-service, solana-api-service,
solana-quoting-service, solana-oracle-service, solana-price-charting,
solana-gas-station, solana-keeper, solana-option-scheduler, solana-mm-bot,
solana-balance-monitor (+ tools/solana-deployment-manager, not deployed).

## Terraform (`rust-backend/infra/`) — operator applies

- `ecr.tf` `local.service_repos` += the 11 deployable service names.
- `secrets.tf`:
  - auto-password (random_password pattern): `solana-token-info`
    (db_password), `solana-auth-service` (jwt_secret).
  - hand-filled placeholders (REPLACE_ME + ignore_changes):
    `solana-gas-station` (keypair), `solana-scheduler` (keypair),
    `solana-keeper` (keypair + optional pyth api key), `solana-mm-bot`
    (keypair + quote_key), `solana-oracle-service` (pyth api key, optional),
    `solana-price-charting` (database_url), `solana-rpc` (rpc_url — shared
    override, mirrors sui-rpc).
- No SG/EC2/ALB changes (nginx single-entrypoint model).
- **DANGER note carried from memory**: `rust-backend/infra` has local state
  with destructive drift — operator must `terraform plan` and `apply -target`
  the new resources only. Spelled out in the migration doc.

## Per-service values

| service | port(s) | nginx route | DB (wipe-provision) | secret |
|---|---|---|---|---|
| solana-token-info | 9005/9006 | `/{env}/solana-token-info/…` → 9005 | `solana_token_info` | db_password (auto) |
| solana-auth-service | 9007/9008 | `/{env}/solana-auth/…` → 9007 | — | jwt_secret (auto) |
| solana-api-service | 9003 | `/{env}/solana-api/…` → 9003 | — | — |
| solana-quoting-service | 9002 | `/{env}/solana-quoting/…` → 9002 (WS upgrade) | — | — |
| solana-oracle-service | 9013 | none (internal; health via ops only) | — | pyth key (opt) |
| solana-price-charting | 9011 | `/{env}/solana-charts/…` → 9011 (WS) | Tiger (external) | database_url |
| solana-gas-station | 9009 | `/{env}/solana-gas-station/…` → 9009 | — | keypair |
| solana-keeper | 8086 ops | none | — | keypair |
| solana-option-scheduler | 8087 ops | none | `solana_scheduler` | keypair |
| solana-mm-bot | 9010 ops | none | — | keypair+quote_key |
| solana-balance-monitor | 9012 ops | none | — | — |

Health gate paths (deploy.sh `health_path_for`): publicly-routed services use
`/{env}/<svc>/health`; internal-only services are **omitted from the health
map** (deploy.sh skips the gate — the Sui keeper/mm-bot precedent) but get
Gatus checks on their container-DNS ops ports.

## File-by-file

1. `infra/ecr.tf`, `infra/secrets.tf` — above.
2. `Dockerfile.<svc>` × 11 — main-workspace services copy the Sui twin's
   Dockerfile pattern; standalone services copy `Dockerfile.solana-indexer`
   (`--manifest-path services/<svc>/Cargo.toml`).
   solana-token-info's runtime image expects
   `/app/deployments/solana-deployments.json` mounted from the deploy bundle
   (same volume the Sui token-info uses for deployments.json — the bundle
   gains the second file, one line in `_deploy.yml`'s bundle step).
3. `deployment/bake.hcl` — 11 targets + group default.
4. `deployment/affected.py` — ALL_SERVICES + SERVICE_GLOBS (standalone
   services glob their own dir + Dockerfile + runtime-config/observability +
   `crates/solana-*`; main-workspace services also glob shared crate deps;
   solana-tx consumers glob `solana-contracts/programs/**` since program
   crates are path-deps).
5. compose staging+prod — 11 service blocks + nginx depends_on. DB services
   get `DB_PASSWORD`/`DB_HOST`; secret consumers mount `/run/secrets`.
   price-charting-style env: `SOLANA_CHART_DATABASE_URL` sourced via `.env`.
6. `deployment/ec2/deploy.sh` — ALL_SERVICES, tag_var_for
   (`SOLANA_TOKEN_INFO_TAG` …), compose_name_for (identity),
   health_path_for (public ones only).
7. `deployment/ec2/render-secrets.sh` — render blocks: solana-gas-station /
   solana-scheduler / solana-keeper / solana-mm-bot ⇒
   `[solana]\n<network> = "<keypair>"` (+ `[mm_bot] quote_key`,
   `[pyth] api_key`, and appended `rpc_url` from the shared `solana-rpc`
   secret — mirroring the sui-rpc merge); solana-auth-service ⇒
   `[auth] jwt_secret`; solana-oracle-service ⇒ `[pyth] api_key`;
   solana-price-charting ⇒ `.solana_chart_database_url` env file;
   solana-token-info db_password → `.env`.
8. nginx staging+prod — location blocks per the route table (quoting +
   charts with WS upgrade map; solana-token-info admin routes same as
   token-info).
9. `_deploy.yml` — force_all list + deploy-bundle step adds
   `solana-deployments.json`.
10. `wipe-provision-db.yml` + `wipe-provision-db.sh` — `solana-token-info`
    (prefix `solana_token_info`), `solana-option-scheduler`
    (`solana_scheduler`).
11. monitoring — prometheus.yml + prometheus-agent.yml scrape targets
    (every service's metrics port), gatus checks
    (`http://options-<env>-<svc>-1:<port>/health`).
12. `start-service.yml`/`stop-service.yml` — add all 11 **and solana-indexer**
    (fixing the existing gap).

## Operator run-book (goes in the migration doc)

1. `terraform plan` → `apply -target` the new ECR repos + secrets.
2. Fill hand-filled secrets (`aws secretsmanager put-secret-value`) both envs:
   keypairs (generate with `solana-keygen new`), solana-rpc Helius URL,
   Tiger database_url.
3. Merge the branch; run **Deploy staging with force_all=true** (tag seeding).
4. Provision DBs: run wipe-provision-db for `solana-token-info` and
   `solana-option-scheduler` (staging first).
5. Deploy programs to devnet (`solana-contracts/scripts/deploy-devnet.sh`),
   run `solana-deploy -e staging -n devnet --deploy-tokens
   --faucet-authority <gas-station pubkey>`, commit the updated
   `solana-deployments.json`, redeploy solana-token-info.
6. Fund wallets (devnet airdrop / transfer), verify balance-monitor gauges.
