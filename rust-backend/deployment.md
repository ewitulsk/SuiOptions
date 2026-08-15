# Deployment

How the three services in `services/` get from a `git push` to a running
process on AWS. Two environments, one EC2 box (for now), one Aurora
cluster (for now), one ALB. Fully automatic on push; manual contract
publishes; scaling plans for the obvious next splits at the bottom.

---

## 1. Environments

| Env | Sui network | Branch that ships it | DB name | Quoting host port |
|---|---|---|---|---|
| `staging` | testnet | `staging` | `indexer_staging` | 9022 |
| `prod` | mainnet | `main` | `indexer_prod` | 9032 |

Branch → env mapping:

- Push / merge to `staging` → build once, deploy to `staging`.
- Push / merge to `main` → deploy to `prod`. No human gate (per your
  call). The git tag on the deploy commit is the rollback handle.

Naming convention used everywhere in this doc and in code: `staging`,
`prod` for the environment; `testnet`, `mainnet` for the Sui network.
They are not the same word — `staging` env runs against `testnet`,
`prod` runs against `mainnet`.

> A third `dev` env (Sui devnet) used to sit alongside these; it was
> never used and was removed in SO-160.

---

## 2. Architecture

```
                                    Internet
                                       │
                                       ▼
                          ┌──────────────────────────┐
                          │   ALB (HTTPS, ACM cert)  │
                          │   api.<domain>           │
                          │   /staging/* → tg-stg    │
                          │   /prod/*  → tg-prod     │
                          └─────────────┬────────────┘
                                        │
                                        ▼
       ┌─────────────────── EC2 (single box) ─────────────────────┐
       │                                                          │
       │  /opt/options/staging/        /opt/options/prod/         │
       │  docker-compose.yml           docker-compose.yml         │
       │   ├─ indexer   :9021           ├─ indexer   :9031        │
       │   ├─ quoting   :9022◄          ├─ quoting   :9032◄       │
       │   └─ mm-bot    (no port)       └─ mm-bot                 │
       │                                                          │
       │  Volumes (per env):                                      │
       │   secrets_<env>   holds rendered secrets.toml            │
       │                                                          │
       └──────────────────────────┬───────────────────────────────┘
                                  │ Postgres :5432
                                  ▼
                ┌──────────────────────────────────────┐
                │  Aurora Postgres cluster (1 writer)  │
                │  ├─ DB: indexer_staging              │
                │  └─ DB: indexer_prod                 │
                └──────────────────────────────────────┘
```

Inside each env's compose stack the three containers share a private
Docker network (`options_<env>_net`). `mm-bot` talks to
`ws://quoting:9002/`, `quoting` talks to `ws://indexer:9001/`, both by
container name. Only the quoting-service exposes a host port; that's the
single ingress point the ALB connects to.

The `◄` arrows show the only port published outside the compose network
per env.

---

## 3. Repo / image layout

Each service ships as its own Docker image, built from its own
`Dockerfile`. One image is environment-agnostic; the per-env config
files are all baked in, and the entrypoint picks one based on
`$APP_ENV`.

```
rust-backend/
├── Dockerfile.indexer
├── Dockerfile.quoting
├── Dockerfile.mm-bot
├── deployment/
│   ├── compose/
│   │   ├── docker-compose.staging.yml
│   │   └── docker-compose.prod.yml
│   └── ec2-bootstrap.sh
├── services/
│   ├── indexer/config/
│   │   ├── config.staging.toml
│   │   └── config.prod.toml
│   ├── quoting-service/config/
│   │   ├── config.staging.toml
│   │   └── config.prod.toml
│   └── mm-bot/config/
│       ├── config.staging.toml
│       └── config.prod.toml
└── deployments.json          (committed, baked into all three images)
```

### Dockerfile pattern (one per service)

Multi-stage build to keep the runtime image small and to take advantage
of cargo layer caching. The Sui git deps add ~5 min on a cold cache;
once warm in GH Actions it's seconds. Example for the indexer:

```dockerfile
# Dockerfile.indexer
FROM rust:1-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY shared shared
COPY services services
COPY tools tools
COPY tests tests
RUN cargo build --release -p indexer

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /src/target/release/indexer /usr/local/bin/indexer
COPY services/indexer/config/ /app/config/
COPY deployments.json /app/deployments.json
ENTRYPOINT ["/bin/sh", "-c", "exec /usr/local/bin/indexer --config /app/config/config.${APP_ENV}.toml"]
```

Quoting-service and mm-bot follow the same pattern with their own
binary and configs. The mm-bot entrypoint additionally passes
`--secrets /run/secrets/secrets.toml` (see §6).

### Per-env config differences

`config.staging.toml` example for the indexer:

```toml
network                 = "testnet"
deployments_path        = "/app/deployments.json"
remote_store_url        = "https://checkpoints.testnet.sui.io"
concurrency             = 5
fanout_addr             = "0.0.0.0:9001"   # bind on all interfaces; only the compose net can reach it
heartbeat_interval_secs = 5
database_url            = "postgresql://indexer:${DB_PASSWORD}@<aurora-endpoint>:5432/indexer_staging"
db_pool_size            = 8
recent_log_capacity     = 1024
```

Diff from `config.prod.toml`: `network = "mainnet"`, different
`remote_store_url`, different `database_url`, optional `start_checkpoint`.

> **Config loader note.** Two of the values above use `${VAR}`
> interpolation: `${DB_PASSWORD}` in the indexer's `database_url`, and
> nothing else (everything else is constant per env). The config loader
> needs to expand `${VAR}` against process env. If the current loader
> doesn't, that's a one-line addition: run `shellexpand` (or equivalent)
> over the string fields before deserializing.

`bind_addr` / `fanout_addr` / `quoting_url` listen on container-internal
ports (9001 for indexer fanout, 9002 for quoting). The host-side port
mapping in `docker-compose.<env>.yml` is what publishes 9012/9022/9032.

### docker-compose.staging.yml (prod is the same shape)

```yaml
name: options-staging

services:
  indexer:
    image: ${ECR}/indexer:${IMAGE_TAG}
    environment:
      APP_ENV: staging
      DB_PASSWORD: ${DB_PASSWORD}        # injected from deploy step
      RUST_LOG: info,indexer=debug
    restart: unless-stopped
    networks: [options_staging_net]

  quoting:
    image: ${ECR}/quoting-service:${IMAGE_TAG}
    environment:
      APP_ENV: staging
      RUST_LOG: info,quoting_service=debug
    ports:
      - "9022:9002"
    depends_on: [indexer]
    restart: unless-stopped
    networks: [options_staging_net]

  mm-bot:
    image: ${ECR}/mm-bot:${IMAGE_TAG}
    environment:
      APP_ENV: staging
      RUST_LOG: info,mm_bot=debug
    volumes:
      - /opt/options/staging/secrets:/run/secrets:ro
    depends_on: [quoting]
    restart: unless-stopped
    networks: [options_staging_net]

networks:
  options_staging_net:
```

The mm-bot keeps no persistent state: it resolves its Account from chain
state for the current deployment on every boot (scanning `AccountCreated`
events under the current package), so container replacement and contract
redeploys need no volume or file handling.

---

## 4. AWS infrastructure inventory

| Resource | Notes |
|---|---|
| **EC2 instance** | Ubuntu 22.04 LTS, t3.medium minimum (4 GB RAM; the indexer's in-memory state grows with checkpoint lag). Single AZ. Docker + docker compose + SSM Agent installed. IAM instance profile with ECR pull, Secrets Manager read, CloudWatch Logs write. |
| **Aurora Postgres cluster** | Single writer instance (Aurora minimum); 2 logical DBs (`indexer_staging`, `indexer_prod`); 1 DB user per env with grants scoped to its DB only. In private subnet, security group permits EC2 → 5432. |
| **ECR repos** | `options/indexer`, `options/quoting-service`, `options/mm-bot`. Lifecycle policy: keep last 20 images per repo. |
| **ALB** | HTTPS:443, HTTP:80→443 redirect. Single ACM cert for `api.<domain>`. Two target groups (one per env) with health check on `/health` (see §10). |
| **AWS Secrets Manager** | Two secrets per env: `options/<env>/sui-key` (Sui bech32 key), `options/<env>/mm-quote-key` (MM quote key), plus one shared `options/<env>/db-password`. |
| **Route 53** | One A-record alias: `api.<domain>` → ALB. |
| **CloudWatch Logs** | Not used initially. Logs stay in `docker logs` on the host. See §13 for the Grafana/Loki migration plan. |

---

## 5. ALB path routing

One ALB listener on 443. Rules (top to bottom):

1. Path `/staging/*` → target group `tg-quoting-staging` → EC2:9022
2. Path `/prod/*` → target group `tg-quoting-prod` → EC2:9032
3. Default → 404

WebSocket upgrades pass through ALB transparently; no special config.
ACM cert covers `api.<domain>`; renewal is automatic.

> **Wrinkle to fix in the quoting-service.** ALB does **not** strip the
> path prefix before forwarding. A client connecting to
> `wss://api.<domain>/staging/` arrives at the quoting-service as a WS
> upgrade on path `/staging/`. The quoting-service's WS route currently
> binds to `/` only. Two options:
>
> 1. Make the quoting-service accept any path (one line in the router).
> 2. Add a tiny Caddy/nginx sidecar in each compose stack that strips
>    `/<env>` before forwarding to the quoting container.
>
> Recommended: option (1). The service doesn't otherwise care about
> URL paths and binding to any path is the lowest-friction fix.

---

## 6. Secrets

Stored in AWS Secrets Manager, fetched at deploy time, written to a
host-side directory bind-mounted read-only into the mm-bot container.

| Secret name | Consumed by | Format |
|---|---|---|
| `options/<env>/sui-key` | mm-bot (pays gas) | Sui bech32 (`suiprivkey1…`) |
| `options/<env>/mm-quote-key` | mm-bot (signs quotes) | bech32 or raw hex |
| `options/<env>/db-password` | indexer | random 32-char string |

### How they get to the container

The deploy job runs on EC2 (via SSM) and does:

```bash
ENV=staging
mkdir -p /opt/options/$ENV/secrets

aws secretsmanager get-secret-value \
  --secret-id options/$ENV/sui-key \
  --query SecretString --output text > /tmp/sui-key

aws secretsmanager get-secret-value \
  --secret-id options/$ENV/mm-quote-key \
  --query SecretString --output text > /tmp/mm-quote-key

cat > /opt/options/$ENV/secrets/secrets.toml <<EOF
[sui]
$(case $ENV in
  staging)  echo "testnet = \"$(cat /tmp/sui-key)\"" ;;
  prod)     echo "mainnet = \"$(cat /tmp/sui-key)\"" ;;
esac)

[mm_bot]
quote_key = "$(cat /tmp/mm-quote-key)"
EOF

shred -u /tmp/sui-key /tmp/mm-quote-key
chmod 600 /opt/options/$ENV/secrets/secrets.toml
```

The DB password is exported as `$DB_PASSWORD` in the compose env so the
indexer config's `${DB_PASSWORD}` interpolation works:

```bash
export DB_PASSWORD=$(aws secretsmanager get-secret-value \
  --secret-id options/$ENV/db-password \
  --query SecretString --output text)
```

> **Why not have the binaries fetch from Secrets Manager themselves?**
> Lower-stakes failure mode (deploy fails noisily; running services
> don't suddenly lose access on a credential rotation), and it avoids
> putting the AWS SDK in the Rust workspace. Move to in-process
> fetching later if rotation cadence becomes an issue.

---

## 7. Persistent state per env

| Path on host | What | Why it persists |
|---|---|---|
| `/opt/options/<env>/secrets/secrets.toml` | mm-bot Sui + quote keys | Rendered each deploy from Secrets Manager. |
| Aurora `indexer_<env>` DB | Indexer events, checkpoint progress | Authoritative state. Backups: Aurora's automated daily snapshot (7-day retention is the default; bump prod to 30). |
| `deployments.json` (baked into image) | Package ids per network | Updated by manually running `cargo run -p deploy`, committed to repo, picked up on next image build. |

### deployments.json update cycle

1. Developer runs `cargo run -p deploy -- ...` locally targeting a
   specific network. The deploy tool writes `deployments.json`.
2. Developer commits `deployments.json` + pushes to `staging` (for
   testnet redeploys) or `main` (for mainnet).
3. CI builds new images that bake the updated `deployments.json` in.
4. Deploy step rolls services on the EC2.

Contract publishes are **not** in CI — too easy to misfire on mainnet.

---

## 8. GitHub Actions

Two workflows: `.github/workflows/deploy-lower.yml` and
`.github/workflows/deploy-prod.yml`.

### deploy-lower.yml (push to `staging` → staging)

```yaml
name: Deploy lower envs

on:
  push:
    branches: [staging]

permissions:
  contents: read
  id-token: write   # OIDC to AWS

jobs:
  build:
    runs-on: ubuntu-latest
    outputs:
      image_tag: ${{ steps.sha.outputs.tag }}
    steps:
      - uses: actions/checkout@v4
      - id: sha
        run: echo "tag=$(git rev-parse --short=12 HEAD)" >> $GITHUB_OUTPUT

      - uses: aws-actions/configure-aws-credentials@v4
        with:
          role-to-assume: arn:aws:iam::<acct>:role/gh-actions-deploy
          aws-region: us-east-1
      - uses: aws-actions/amazon-ecr-login@v2

      - name: Build & push (parallel via buildx bake)
        run: |
          docker buildx bake -f deployment/bake.hcl --push \
            --set "*.tags=$ECR/options/indexer:${{ steps.sha.outputs.tag }}" \
            --set "*.tags=$ECR/options/quoting-service:${{ steps.sha.outputs.tag }}" \
            --set "*.tags=$ECR/options/mm-bot:${{ steps.sha.outputs.tag }}"

  deploy:
    needs: build
    strategy:
      matrix:
        env: [staging]
      fail-fast: false
    runs-on: ubuntu-latest
    steps:
      - uses: aws-actions/configure-aws-credentials@v4
        with:
          role-to-assume: arn:aws:iam::<acct>:role/gh-actions-deploy
          aws-region: us-east-1

      - name: SSM deploy
        run: |
          aws ssm send-command \
            --instance-ids i-XXXXXXXX \
            --document-name AWS-RunShellScript \
            --parameters commands="[
              'cd /opt/options/${{ matrix.env }}',
              'export IMAGE_TAG=${{ needs.build.outputs.image_tag }}',
              './deploy.sh ${{ matrix.env }}'
            ]" \
            --output text
```

The deploy pulls the image SHA the build job just pushed — that's the
whole point of the staging-branch mapping.

### deploy-prod.yml (push to `main` → prod)

Same shape, single non-matrixed job. No GH environment approval gate
(per your call); rollback is by re-running the workflow with a previous
SHA via `workflow_dispatch` (see §11).

### EC2-side deploy.sh

A short script in `/opt/options/<env>/deploy.sh`. The SSM command above
invokes it. This is the place to do health checks and rollbacks.

```bash
#!/usr/bin/env bash
set -euo pipefail
ENV=$1
COMPOSE=docker-compose.${ENV}.yml
PREV_TAG=$(grep '^IMAGE_TAG=' .env | cut -d= -f2 || echo "")

# Render secrets (see §6).
./render-secrets.sh "$ENV"

# Pull DB password and write to .env.
DB_PASSWORD=$(aws secretsmanager get-secret-value \
  --secret-id options/$ENV/db-password \
  --query SecretString --output text)

# Atomic .env swap so a failed pull doesn't leave half-state.
cat > .env.new <<EOF
ECR=<acct>.dkr.ecr.us-east-1.amazonaws.com
IMAGE_TAG=${IMAGE_TAG}
DB_PASSWORD=${DB_PASSWORD}
EOF
mv .env.new .env

aws ecr get-login-password --region us-east-1 \
  | docker login --username AWS --password-stdin <acct>.dkr.ecr.us-east-1.amazonaws.com

docker compose -f "$COMPOSE" pull
docker compose -f "$COMPOSE" up -d --remove-orphans

# Health check — give services 30s to come up.
sleep 30
PORT=$(case $ENV in staging) echo 9022;; prod) echo 9032;; esac)
if ! curl -fsS "http://localhost:$PORT/health" >/dev/null; then
  echo "Health check failed; rolling back to $PREV_TAG"
  if [ -n "$PREV_TAG" ]; then
    sed -i "s/^IMAGE_TAG=.*/IMAGE_TAG=$PREV_TAG/" .env
    docker compose -f "$COMPOSE" up -d
  fi
  exit 1
fi
```

> **Needs a `/health` endpoint.** The quoting-service should expose a
> trivial `GET /health` returning `200 OK` once it's connected to the
> indexer. If absent today, add it before the first deploy — it's the
> only signal the deploy script has to know whether to roll back.

---

## 9. One-time bootstrap

Done once per AWS account. Not in CI.

1. **VPC & networking.** Default VPC is fine. Two private subnets for
   Aurora (multi-AZ requirement even on single-instance clusters),
   public subnets for the ALB, EC2 in either (private + NAT gateway
   recommended; public + restricted SG is acceptable for early-stage).
2. **EC2.** Launch a t3.medium with the IAM instance profile attached
   (ECR pull, Secrets Manager read, CloudWatch Logs write, SSM core).
   Run `deployment/ec2-bootstrap.sh` once:
   - Install Docker, docker compose v2, AWS CLI v2, SSM Agent.
   - `mkdir -p /opt/options/{staging,prod}/{secrets}`.
   - Copy `docker-compose.<env>.yml` and `deploy.sh` into each dir.
3. **Aurora cluster.** Aurora Postgres 16, single writer
   (`db.t4g.medium`). Create master user. Then via psql:
   ```sql
   CREATE DATABASE indexer_staging;
   CREATE DATABASE indexer_prod;
   CREATE USER indexer_staging WITH PASSWORD '<from-secrets-manager>';
   CREATE USER indexer_prod    WITH PASSWORD '<from-secrets-manager>';
   GRANT ALL PRIVILEGES ON DATABASE indexer_staging TO indexer_staging;
   GRANT ALL PRIVILEGES ON DATABASE indexer_prod    TO indexer_prod;
   ```
   Migrations are embedded in the service binaries and run on first connect.

   > The `scheduler_staging` / `scheduler_prod` databases belonged to the
   > decommissioned option-scheduler. Nothing reads them; drop them at your
   > convenience.
4. **ECR.** Create the three repos. Set lifecycle policy to keep last 20.
5. **Secrets Manager.** Create the secrets listed in §4.
6. **ACM.** Request a public cert for `api.<domain>`, validate via DNS.
7. **ALB.** Create with HTTPS:443 listener; two target groups, two
   path rules, one health check per TG. Default action returns 404.
8. **Route 53.** Alias `api.<domain>` → ALB.
9. **GitHub OIDC.** Create the `gh-actions-deploy` IAM role with a trust
   policy scoped to the repo and the two deploy branches. Permissions:
   ECR push, SSM `SendCommand` on the EC2 instance, nothing else.

---

## 10. Day-2 ops

### Tail logs

```bash
ssh / SSM-start-session to EC2
cd /opt/options/<env>
docker compose logs -f indexer
docker compose logs -f quoting
docker compose logs -f mm-bot
```

### Restart a single service

```bash
cd /opt/options/<env>
docker compose restart quoting
```

### Roll back to a previous image SHA

Re-run the GH Actions deploy workflow via `workflow_dispatch` with a
chosen tag, **or** ad-hoc on the box:

```bash
cd /opt/options/<env>
sed -i 's/^IMAGE_TAG=.*/IMAGE_TAG=<old-sha>/' .env
docker compose pull
docker compose up -d
```

### Redeploy the Move contract

```bash
# Local dev machine, against the appropriate network.
cargo run -p deploy -- ... --network testnet
git add deployments.json
git commit -m "redeploy testnet"
git push origin staging   # or main for mainnet
```

CI picks up the change, rebuilds images with the new `deployments.json`
baked in, and rolls services.

### Reset an env

(useful on staging when something gets stuck)

```bash
cd /opt/options/<env>
docker compose down -v       # -v drops the mm-bot state volume
# Aurora DB: connect via psql and DROP/RECREATE the env's database.
docker compose up -d
# mm-bot will bootstrap a fresh Account on next start.
```

---

## 11. Adding a new service

Recipe for taking a new long-running process from `cargo new` to running
in both envs. Stick to it and the existing deploy pipeline picks
up the new service with no manual one-offs.

Worked example: a new service called `oracle-bridge`.

### 1. Create the crate

```
cd services
cargo new --lib oracle-bridge
```

In `services/oracle-bridge/Cargo.toml`, set up the binary the same way
the others do (`[[bin]] name = "oracle-bridge"`, depend on `shared`,
register via `shared::define_program!` if you want it to appear in the
control-panel TUI).

### 2. Wire into the workspace

`rust-backend/Cargo.toml`:

```toml
[workspace]
members = [
    "shared",
    "services/indexer",
    "services/quoting-service",
    "services/mm-bot",
    "services/oracle-bridge",        # <-- add
    ...
]
```

### 3. Create per-env config files

Mirror the pattern used by the existing services:

```
services/oracle-bridge/config/
├── config.staging.toml
└── config.prod.toml
```

If the service needs secrets, also list which secrets it expects in a
header comment at the top of `config.staging.toml`. Don't commit a
`secrets.toml`; the deploy pipeline renders it from Secrets Manager
(see step 7).

### 4. Add a Dockerfile

`Dockerfile.oracle-bridge` at the repo root, same multi-stage pattern
as the others:

```dockerfile
FROM rust:1-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY shared shared
COPY services services
COPY tools tools
COPY tests tests
RUN cargo build --release -p oracle-bridge

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /src/target/release/oracle-bridge /usr/local/bin/oracle-bridge
COPY services/oracle-bridge/config/ /app/config/
COPY deployments.json /app/deployments.json
ENTRYPOINT ["/bin/sh", "-c", "exec /usr/local/bin/oracle-bridge --config /app/config/config.${APP_ENV}.toml"]
```

Adjust the entrypoint if the service takes a secrets file
(`--secrets /run/secrets/secrets.toml`).

### 5. Register the image in the build pipeline

`deployment/bake.hcl` (or the `docker buildx bake` config equivalent):

```hcl
target "oracle-bridge" {
  dockerfile = "Dockerfile.oracle-bridge"
  tags       = ["${ECR}/options/oracle-bridge:${IMAGE_TAG}"]
}

group "default" {
  targets = ["indexer", "quoting-service", "mm-bot", "oracle-bridge"]
}
```

`group "default"` is what the GH Actions workflow builds; adding the
new target there is the only CI change needed. The deploy step does
not need an update — it just runs `docker compose up` for whatever's
declared in the env's compose file.

### 6. Add to each env's compose file

`deployment/compose/docker-compose.staging.yml` (and `.prod.yml`):

```yaml
services:
  oracle-bridge:
    image: ${ECR}/options/oracle-bridge:${IMAGE_TAG}
    environment:
      APP_ENV: staging      # prod in prod file
      RUST_LOG: info,oracle_bridge=debug
    depends_on: [indexer]   # if it reads from the indexer
    restart: unless-stopped
    networks: [options_staging_net]
    # Only add ports: / volumes: / secrets if needed (see 8 / 9).
```

Two near-identical edits, one per env file. Keep them in sync.

### 7. (If it needs secrets) Add to Secrets Manager + render script

For each env, create the secret:

```
aws secretsmanager create-secret \
  --name options/staging/oracle-bridge-api-key \
  --secret-string '<...>'
# Repeat for prod.
```

Then extend `/opt/options/<env>/render-secrets.sh` to fetch the new
secret and append to (or generate a separate) `secrets.toml` under
`/opt/options/<env>/secrets/`. Bind-mount that directory into the new
container in step 6 (`/opt/options/<env>/secrets:/run/secrets:ro`).

### 8. (If it needs persistent state) Add a named volume

```yaml
services:
  oracle-bridge:
    volumes:
      - oracle_bridge_dev_state:/app/state

volumes:
  oracle_bridge_dev_state:
```

Make sure the binary writes its state under `/app/state/` (CLI flag or
config option).

### 9. (If it's externally reachable) Add ALB plumbing

If the new service exposes an HTTP/WS endpoint that needs to be hit
from the internet, repeat the §5 pattern:

1. Pick a per-env host port (e.g. 9023/9033 — leave room above
   the existing 9021-9032 block).
2. `ports: ["9023:<container-port>"]` in each compose file.
3. Create a new ALB target group per env (`tg-oracle-bridge-staging`
   etc.), register the EC2 instance on the new port, health check on
   `/health`.
4. Add an ALB path rule per env (e.g. `/staging/oracle/*` →
   tg-staging). Same
   path-prefix wrinkle from §5 applies — the service must accept any
   URL path, or grow a Caddy strip-prefix sidecar.

If it's purely internal (talks only to indexer/quoting/mm-bot via the
compose network), skip this step entirely.

### 10. Add a `/health` endpoint

The deploy script's health check in §8 only covers the quoting-service
today. If the new service is critical, extend `deploy.sh` to curl its
`/health` too and roll back together if either fails. If it's
non-critical (e.g. a batch poller), let it crash-loop visibly rather
than triggering a rollback.

### 11. Ship it

```
git checkout -b add-oracle-bridge
# all of the above
git push origin add-oracle-bridge
# open PR into staging
```

Once merged into `staging`:

1. GH Actions builds and pushes four images (the new one included).
2. SSM rolls staging.
3. `docker compose logs -f oracle-bridge` on the EC2 to confirm it
   came up.

When you're happy, merge `staging` → `main` and the prod env picks it
up the same way.

### Checklist (no-context skim version)

- [ ] Crate created under `services/<name>/`
- [ ] Added to root `Cargo.toml` workspace members
- [ ] Per-env config files committed
- [ ] `Dockerfile.<name>` at repo root
- [ ] Added to `deployment/bake.hcl` default group
- [ ] Added to both `docker-compose.<env>.yml` files
- [ ] Secrets created in Secrets Manager (if needed) + render script updated
- [ ] Named volume defined (if it has state)
- [ ] ALB target group + path rule (if externally reachable)
- [ ] `/health` endpoint and `deploy.sh` extended (if critical)

---

## 12. Known wrinkles / TODOs

- **`/health` endpoint** on quoting-service: add before first deploy
  (see §8).
- **WS path handling**: quoting-service must accept any URL path so ALB
  path-prefix routing works (see §5).
- **Config env-var expansion**: confirm the config loader expands
  `${DB_PASSWORD}` (or switch to reading the DB URL directly from an
  env var the binary already understands).
- **`start_checkpoint`** for prod (mainnet) needs to be picked — leave
  unset to tail from tip, or pick a specific checkpoint for backfill.
- **mm-bot bootstrap on first prod deploy**: the bot mints test tokens
  from a faucet during bootstrap. On mainnet there is no faucet, so the
  prod mm-bot needs a different bootstrap path (manual funding, or a
  config flag to skip the mint step). Do not deploy mm-bot to prod
  until that's resolved.

---

## 13. Scaling plan A — split prod onto its own EC2

Trigger: prod traffic / mm-bot activity starts to push the shared box,
**or** a staging deploy bug ever takes prod down.

Goal: staging stays on the existing box; prod moves to its own
EC2. No code changes; only infra + a tweak to the prod deploy job's
target.

### Steps

1. **Provision a new EC2** (`options-prod-ec2`), same AMI/role as the
   shared box. Run `ec2-bootstrap.sh` to install Docker, compose, SSM,
   and create `/opt/options/prod/`.
2. **No mm-bot state to copy.** The mm-bot resolves its Account from chain
   state for the current deployment on boot, so there is no per-host file
   or volume to migrate — it will re-adopt the same on-chain Account from
   the new box.
3. **Cut over the ALB.** Detach the existing EC2 from `tg-quoting-prod`,
   register the new EC2 instead. Drain delay on the TG handles in-flight
   WS connections (set to 30s).
4. **Stop the prod compose stack on the shared box.** `docker compose
   -f docker-compose.prod.yml down`.
5. **Update GH Actions.** The prod deploy workflow's
   `--instance-ids` value changes from the shared box to the new one.
   One-line change.
6. **First deploy on the new box** to confirm the loop closes.

### What stays the same

ECR, Aurora (still shared), Secrets Manager paths, ALB host, image
build pipeline. Only the SSM target ID changes.

### Cost delta

One additional t3.medium ($30/mo) and an EBS volume ($5/mo). Worth it
the day prod and staging share a kernel and a bad commit takes both down.

---

## 14. Scaling plan B — split prod onto its own Aurora cluster

Trigger: prod query patterns start interfering with staging
performance, **or** you want a stricter blast-radius story for the
prod DB.

Goal: prod gets its own Aurora cluster. staging continues to
use the original cluster.

### Steps

1. **Provision** a new Aurora Postgres cluster, `options-prod-db`,
   matching version of the shared cluster's engine. Single writer to
   start; add a reader replica later if needed.
2. **Create the prod DB & user** on the new cluster (`indexer_prod` +
   `indexer_prod` user, password from a new Secrets Manager entry).
3. **Stop the prod indexer** to freeze state:
   `docker compose -f docker-compose.prod.yml stop indexer`. Quoting +
   mm-bot can keep running but RFQs against expired state will be
   useless — schedule a maintenance window.
4. **Migrate data.** Two options:
   - **`pg_dump` / `pg_restore`** (simple, requires the downtime above):
     ```
     pg_dump --no-owner --no-acl -d indexer_prod -h <old-endpoint> \
       | psql -d indexer_prod -h <new-endpoint>
     ```
   - **Logical replication** (no downtime): publish `indexer_prod` on
     old, subscribe on new; cut over when caught up. More moving parts.
   For an early-stage indexer with a couple of GB of events, `pg_dump`
   is fine — a 5-minute maintenance window costs nothing.
5. **Update** `options/prod/db-password` (new password) in Secrets
   Manager, and update prod's `config.prod.toml` `database_url` to the
   new Aurora endpoint. Commit, push to `main`. CI rebuilds the prod
   image and rolls services.
6. **Drop the old prod DB** from the shared cluster:
   `DROP DATABASE indexer_prod; DROP USER indexer_prod;`. Audit the
   shared cluster's SG to confirm prod EC2 is no longer in the allow
   list.

### What stays the same

EC2, ALB, ECR, image build pipeline, Sui contract setup. Only the
indexer's database connection string changes.

### Cost delta

A second Aurora cluster's writer node (~$60/mo for `db.t4g.medium`).
Combine with plan A if you want full prod isolation in one stroke.

---

## 15. Observability (implemented — SO-180)

Logs (Loki), metrics (Prometheus), traces (Tempo), all in the Grafana on
the shared host (`https://<domain>/grafana/`). Originally "scaling plan C";
implemented in SO-180.

### Architecture

```
   Grafana ── Loki (logs) ─ Prometheus (metrics) ─ Tempo (traces)
                ▲                  ▲                   ▲
   shared host: │ promtail         │ scrape over       │ OTLP 4318 over
                │ (docker SD)      │ options-staging_  │ options-staging_net
                │                  │ net DNS           │
   prod host:   │ promtail         │ prom-agent        │ services push OTLP
                │ push :3100       │ remote-write      │ to <shared-ip>:4318
                │ (VPC, SG-gated)  │ :9090 (VPC)       │ (VPC, SG-gated)
```

Every service initializes through `crates/observability`
(`observability::init("<service>")`): JSON logs when stdout isn't a TTY
(`OBS_LOG_FORMAT` overrides), a Prometheus recorder served at `/metrics`
(either on the axum router's internal port or the ops server that replaced
the old health server), and an OTel layer exporting OTLP/HTTP when
`OTEL_EXPORTER_OTLP_ENDPOINT` is set (deploy.sh writes `OTEL_ENDPOINT`
into each env's `.env`: `http://tempo:4318` on the shared host, the
central Tempo's VPC address on the prod host).

### Conventions

- **Alerting from code**: `error!(alert_id = "stable-id", ...)` anywhere
  fires the provisioned "Tagged error logs" Grafana rule within ~1 min,
  grouped by `alert_id`. No infra change per alert. Test the pipeline
  end-to-end with `ALERT_TEST=1` on balance-monitor, which emits
  `alert_id = "this-is-a-test-alert"` at boot.
- **HTTP servers** get `http_requests_total` / `http_request_duration_seconds`
  / `http_requests_in_flight` (method × route × status) plus a per-request
  span continuing the caller's W3C `traceparent`; the trace id is stamped
  on every log line inside the request and echoed as `x-trace-id`.
- **Inter-service calls** go through `observability::client::instrumented`
  in the four client crates — they propagate `traceparent` (so Tempo shows
  frontend → api-service → indexer → DB in one waterfall) and record
  `client_request*` metrics.
- **Indexer DB calls** are wrapped in `db_query` spans (Diesel work inside
  `spawn_blocking` re-enters the request span) + an
  `indexer_db_query_duration_seconds{query}` histogram — open any slow
  GraphQL trace in Tempo to see exactly which query burned the time.
- **Wallet balances**: the `balance-monitor` service polls the gas-station /
  deployer / mm-bot wallets (addresses derived from the same rendered
  secrets the services mount; keeper = one more `[[watch]]` in its config)
  and exports `sui_balance_sui` / `sui_balance_low`; the "Low SUI balance"
  rule alerts on the latter.

### Contact points

Alert RULES are provisioned from
`deployment/monitoring/grafana-alerting.yml`; contact points and
notification policies are deliberately UI-managed (they persist in the
grafana data volume) — set them up under Alerting → Contact points.

### Dashboards

`deployment/monitoring/dashboards/*.json`, deployed via the Grafana API
(no host bounce):

```
GRAFANA_URL=https://<domain>/grafana GRAFANA_PASSWORD=... \
  deployment/monitoring/push-dashboards.sh
```

### Rolling config changes to EXISTING hosts

The monitoring configs feed cloud-init `user_data`, which only runs at
first boot (editing it via Terraform would bounce the hosts). To apply
changes to a live host, copy the changed files from
`deployment/monitoring/` to `/opt/options/monitoring/...` via SSM, then:

- shared host: append `PROMETHEUS_*`/`TEMPO_*` paths as laid out in
  `cloud-init.sh.tftpl` (configs under `prometheus/config/`,
  `tempo/config/`, grafana provisioning under
  `grafana/provisioning/{datasources,alerting}/`), then
  `/opt/options/monitoring/monitoring-up.sh` (+ `docker compose restart
  grafana` for provisioning changes).
- prod host: prom-agent config under `prom-agent/config/`, add
  `REMOTE_WRITE_URL=http://<shared-ip>:9090/api/v1/write` to
  `/opt/options/monitoring/.env`, write the central Tempo address to
  `/opt/options/prod/otel-endpoint`, then bring up
  `docker-compose.prom-agent.yml` (needs `options-prod_net`, i.e. after a
  prod deploy).

Keep the repo files and the hosts in sync — the repo copy is what new
hosts boot with.
