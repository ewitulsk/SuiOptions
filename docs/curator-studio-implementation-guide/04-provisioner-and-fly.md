# 04 — P1: `provisioner` + Fly.io bot runtime

The control plane the deploy button drives. New crate `rust-backend/services/provisioner`, port **9024**, internal + dashboard-facing (JWT-gated routes proxied via nginx). Owns the studio control-plane DB: vaults, bots, spec versions, consents, Fly machine records.

## 1. Deploy orchestration (one state machine)

`POST /v1/deploys { specId, specVersion, reportId, consent: {...} }` runs an idempotent, resumable state machine (`deploys` table row per step; every step is retry-safe because each checks its postcondition first):

| Step | Action | Notes |
|---|---|---|
| 1 `key` | key-service `POST /internal/keys` → curator address | |
| 2 `gas` | transfer SUI from the deployer wallet to the curator address | `sui_tx` transfer; address-balance is fine (SO-366). Amount: `initial_gas_sui` config (staging default 2 SUI). Mind the staging gas famine — consolidate deployer coins first if budget-gate errors appear |
| 3 `vault` | with the curator key (via key-service `/internal/sign/tx`): `vault::create_vault<T>` | records `vault_id`, raw `curator_cap` id |
| 4 `wrap` | `bounded_curator::wrap(cap, policy_from_spec, owner=user_wallet)` | mints OwnerCap → user wallet, shares limiter; policy fields come from the spec's `[risk]` block (spec §7.1) |
| 5 `custody` | `guarded_exchange_adapter::init_direct_custody` + `add_signer(curator_address)` per spec market | the curator key is its own approved signer |
| 6 `bind` | key-service `/internal/keys/:id/bind { vaultId }`; issue bot token (bot-gateway `POST /admin/bots`) | |
| 7 `fly` | create Fly app + secrets + machine (§3) | |
| 8 `register` | write `bots` row RUNNING(WAITING_FOR_FUNDS); dashboard picks it up | |

Consent precondition: step 0 verifies `(user, specId, specVersion, reportId)` has a recorded consent row (spec D3/D13) and that the report's spec hash matches the current version. Refuse otherwise.

Teardown (`POST /v1/bots/:id/teardown`): watermark raise + soft-cancel-all via gateway → stop machine → destroy machine + app → mark bot DEAD. Vault and caps are untouched (on-chain state belongs to the user).

Gas top-up loop: hourly, `balance()` of each live curator address; below `min_gas_sui`, transfer top-up; failures `error!(alert_id = "tx-failed-provisioner-gas-topup", …)`.

## 2. Fly.io integration — decisions

Verified against fly.io/docs (2026-08):

- **API**: `https://api.machines.dev` (in-Fly alternative `http://_api.internal:4280`), `Authorization: Bearer <token>`. Resources: Apps → Machines → Volumes. **Rate limit ≈ 1 req/s per action (burst 3)** — the provisioner serializes Fly calls through one queue; a deploy touches ~6 actions, fine.
- **Token**: one **org-scoped token** (`fly tokens create org --expiry …`) for the org `curator-studio-<env>`, stored in the provisioner's secrets TOML (`[fly] api_token`). Narrower per-app deploy tokens don't work for *creating* apps; the org token is the documented choice for programmatic tenant provisioning. Rotate on a calendar.
- **Topology: one Fly app per bot**, name `cs-<env>-<vault_short_hex>` (app names are global — the env prefix prevents collisions). Per-app isolation gives us: secrets scoped per bot (Fly secrets are app-scoped, injected as env vars at machine boot), clean teardown (`DELETE /v1/apps/{name}` removes everything), and per-bot metrics grouping. One machine per app, `shared-cpu-1x` / 256 MB, `restart: { policy: "always" }` — always-on trading loop, no autostop. No public services block (outbound-only: the bot dials the gateway; nothing dials the bot).
- **Region**: pin one region close to the EC2 stack (`iad` if the stack is us-east-1) — latency to the gateway dominates.
- **Image delivery**: build `bot-runtime` in CI and push to Fly's registry under a dedicated builds app: `registry.fly.io/cs-<env>-runtime:<tag>` (`flyctl auth docker` + plain `docker push` in the workflow, authed by the same org token). Machines in other apps within the org reference that image. This avoids wiring ECR pull credentials into Fly. Keep the tag pinned per bot row (`bots.image_tag`) so restarts are reproducible and upgrades are explicit.

Machine config JSON the provisioner submits (`POST /v1/apps/{app}/machines`):

```json
{
  "name": "bot",
  "region": "iad",
  "config": {
    "image": "registry.fly.io/cs-staging-runtime:v12",
    "guest": { "cpu_kind": "shared", "cpus": 1, "memory_mb": 256 },
    "restart": { "policy": "always" },
    "env": { "BOT_MODE": "live" }
  }
}
```

Secrets (set via `POST /v1/apps/{app}/secrets` before machine create, so they're present at first boot): `GATEWAY_URL` (public nginx URL, e.g. `https://sui-options.com/staging/bot-gateway`), `BOT_API_TOKEN`, `SPEC_JSON` (the full spec snapshot), `VAULT_ID`. **No key material — ever** (spec D11).

Restart/monitor: Fly's `restart: always` handles process death; the gateway's heartbeat monitor handles logical death (wedged loop) — on 3 missed heartbeats it alerts `bot-heartbeat-missed` and can `POST /v1/apps/{app}/machines/{id}/restart`. Restart-looping (3 restarts/hour) escalates to `bot-restart-loop` and flips the bot to PAUSED for human attention.

## 3. Dashboard/user-facing API (JWT via nginx)

```
POST /v1/deploys                      (above)
GET  /v1/bots?owner=0x…               list with state + heartbeat freshness
POST /v1/bots/:id/pause|resume|kill   → gateway control plane
POST /v1/bots/:id/teardown
GET  /v1/specs/:id / POST /v1/specs   spec CRUD + versioning (bumps version, stores JSONB + hash)
POST /v1/consents                     records the typed acknowledgment (user, specVersion, reportId, text, ts)
GET  /v1/vaults/:id/authority         guard status: current curator_cap vs guard inner (revoked detection)
```

## 4. DB schema (`provisioner_<env>`)

```
specs          (id, owner_addr, name, created_at)
spec_versions  (spec_id, version, body JSONB, body_hash, created_by, created_at)  PK (spec_id, version)
consents       (id, user_addr, spec_id, spec_version, report_id, text_hash, created_at)
deploys        (id, spec_id, spec_version, state, step, error, created_at, updated_at)
vaults         (vault_id PK, owner_addr, guard_id, limiter_id, curator_addr, key_id, created_at)
bots           (id, vault_id, fly_app, fly_machine_id, image_tag, state, created_at)
```

(The test-runner references `spec_versions` by `(spec_id, version, body_hash)` and embeds the body snapshot in each report — reports stay immutable even if this DB is lost.)

## 5. Bot runtime image

`bot-runtime/Dockerfile` (new top-level dir alongside the SDK, chapter 05):

```dockerfile
FROM python:3.12-slim
WORKDIR /app
COPY python-sdk/ /app/python-sdk/
RUN pip install --no-cache-dir /app/python-sdk
COPY bot-runtime/entry.py /app/entry.py
# SPEC_JSON, GATEWAY_URL, BOT_API_TOKEN, VAULT_ID, BOT_MODE arrive as Fly secrets/env
ENTRYPOINT ["python", "/app/entry.py"]
```

`entry.py` = the template bot (05 §3) parameterized entirely by env. One image serves every v1 bot because strategies are spec + primitives; the bespoke-code path (P4) builds per-bot images instead (08 §1).

## 6. Sponsored-PTB templates for the dashboard

The user's wallet signs three new PTB shapes (deposit is already covered by the existing `vault:deposit` template family). Add to `crates/sui-tx/src/tx/template.rs::protocol_templates` (follow the 7-step recipe in the file, thread a `bounded_curator: Option<ObjectID>` param, tests included):

- `bounded_curator:unwrap`
- `bounded_curator:rotate_curator`
- `bounded_curator:set_policy`

## 7. Alert ids

`tx-failed-provisioner-deploy` (any on-chain step of the state machine), `tx-failed-provisioner-gas-topup`, `provisioner-fly-api-error`, `bot-restart-loop`.
