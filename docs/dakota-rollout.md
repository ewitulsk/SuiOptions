# Dakota integration — rollout

What an operator has to do by hand before and after this ships. The code cannot
do any of it: databases, secrets and ECR repos are provisioned out of band, and
`deploy.sh` health-gates every service it plans.

Behaviour verified against the live sandbox lives in
[dakota-sandbox-notes.md](dakota-sandbox-notes.md). This file is only the
runbook.

---

## 1. Blocking: `auth_prod` must exist before the next prod deploy

**auth-service gained a hard Postgres dependency.** It became a multi-method
identity service (username+password *or* Sui wallet, linkable to one account),
and the store is Postgres. It will not boot without it.

auth-service ships to **prod**, is health-gated, and `deploy.sh` rolls back the
**whole planned set** on the first failed gate. So a prod deploy without this
database does not just fail auth-service — it reverts everything deployed
alongside it.

The embedded migrations run themselves on boot. The database and role do not.

```sql
-- prod RDS
CREATE ROLE auth_prod LOGIN PASSWORD '<the shared DB_PASSWORD>';
CREATE DATABASE auth_prod OWNER auth_prod;
```

The Dakota work this came from is staging-only, but auth-service is shared, so
prod carries the dependency regardless.

## 2. The other two databases

```sql
-- staging RDS
CREATE ROLE auth_staging   LOGIN PASSWORD '<the shared DB_PASSWORD>';
CREATE DATABASE auth_staging   OWNER auth_staging;
CREATE ROLE dakota_staging LOGIN PASSWORD '<the shared DB_PASSWORD>';
CREATE DATABASE dakota_staging OWNER dakota_staging;
```

## 3. Secrets Manager

Create `options/staging/dakota-service`. `render-secrets.sh` writes it to
`/run/secrets/dakota-service.toml`; it **silently skips an absent secret**, and
the container then crash-loops on the missing file.

```toml
[dakota]
# From platform.sandbox.dakota.xyz. Shown once.
api_key = "..."

# Optional — only the treasury needs it. Everything else works without it.
#   openssl ecparam -name prime256v1 -genkey -noout -out p256.pem
#   openssl pkcs8 -topk8 -nocrypt -in p256.pem
wallet_p256_pem = """
-----BEGIN PRIVATE KEY-----
...
-----END PRIVATE KEY-----
"""
```

There is **no `options/prod/dakota-service`**, and there should not be: the
service is not declared in the prod compose file.

## 4. ECR repo

`infra/ecr.tf` gained `dakota-service`. Apply before the first image push — a
missing repo fails the push with a 403, not a useful error.

```sh
cd rust-backend/infra && terraform plan && terraform apply
```

## 5. Deploy, then register the webhook

Deploy staging. Then, **once**, from the dashboard's Ops screen (or
`POST /staging/dakota/admin/webhooks/register` with an admin token):

Nothing appears in the activity feed until a target is registered. Registration
is deliberately manual rather than at boot — registering on every restart churns
targets, and the URL depends on how the environment is proxied.

If events were missed (target registered late, downtime past Dakota's 48-hour
retry window), **Resync** replays `GET /events` through the same extractor.
Events are keyed by id, so replaying cannot double-count. It reports
`truncated` when Dakota had more than one page — run it again rather than
assuming a partial backfill was complete.

## 6. Dashboard

New Vercel project rooted at `dakota-dashboard/`. `vercel.json` carries the SPA
rewrite.

```
VITE_DAKOTA_API = https://sui-options.com/staging/dakota
VITE_AUTH_API   = https://sui-options.com/staging/auth
```

Then add the deployment origin to `allowed_origins` in
`services/dakota-service/config/config.staging.toml` and
`services/auth-service/config/config.staging.toml`.

## 7. First admin

There is no self-serve signup. The first admin bootstraps from the
`admin_addresses` allowlist in auth-service's config: an allowlisted Sui wallet
is auto-provisioned as an admin on first login. That is the **only**
account-creation path that skips an invite — treat the list as a root-of-trust.

Everyone else arrives through an invite:

```
admin → creates a partner business → copies its signup link
      → business registers → invites its own customers
admin → creates an individual directly → copies its signup link
```

Password recovery does not exist, because no email is stored. Recovery is an
admin minting a fresh invite.

---

## Staging-only, and how that is enforced

`deploy.sh` filters the requested set against `docker compose config --services`
for the target environment. A service absent from that file can never be planned
or health-gated. That is the same mechanism excluding `cctp-relay`, `market-sim`,
`twitter-service` and `social-bot` from prod.

For `dakota-service` this is by design rather than circumstance — it integrates
Dakota's **sandbox** (testnet custody, mocked banking, a $2 per-transaction cap),
so there is nothing useful it could do in prod. Four things keep it out, and all
four have to be undone deliberately:

| | |
|---|---|
| `docker-compose.prod.yml` | not declared (with a comment saying why) |
| `nginx.prod.conf` | no route |
| `config.prod.toml` | does not exist — the image would exit on the missing file |
| `options/prod/dakota-service` | no secret |

Verify after any deploy change:

```sh
python3 deployment/test_affected.py                                   # 20 tests
python3 deployment/affected.py rust-backend/services/dakota-service/src/main.rs
# → ["dakota-service"]
grep -c '^  dakota-service:' deployment/compose/docker-compose.prod.yml  # → 0
```

---

## A security change that came with this

auth-service now issues tokens to **business** and **individual** roles, not
only admins. `token-info`'s mutate routes were gated on `require_auth`, which
only proves a token is *valid* — so any newly-created customer account would
have been able to mutate the token catalog.

`crates/auth-client` gained `require_admin`, and `token-info` uses it. Anything
else that gates a privileged operation on `require_auth` wants the same
treatment.

---

## Verifying it works

```sh
# unit + integration
cargo test -p dakota-service -p auth-service -p auth-client        # 91
AUTH_TEST_DATABASE_URL=postgres://…/auth_test \
  cargo test -p auth-service -- --ignored                          # 12

# against the live sandbox
DAKOTA_TEST_API_KEY=… cargo test -p dakota-service -- --ignored live

# whole story, against running services
AUTH=… AUTHI=… DK=… rust-backend/services/dakota-service/smoke.sh   # 31
```

`smoke.sh` covers admin bootstrap, the three-tier hierarchy, cross-scope
isolation, the approval gate, all three ramps, the catalog and network
allow-lists, the $2 cap, sandbox funding, the ledger, and webhook authenticity.

The live signing test is worth understanding: an **insufficient-balance**
rejection is *success*. It means the signature verified and Dakota reached
policy evaluation. `endorsement validation failed` is the failure — and it names
nothing, which is why two undocumented signing rules cost real debugging time
(see the sandbox notes).

---

## The no-PII policy, and how to not break it

Dakota responses are full of PII — `GET /customers` returns `email` and `name`,
`POST /accounts` returns `bank_account.account_holder_name` and
`account_number`, `GET /events` returns `sender_details`. None of it is stored.

Three rules hold the line:

1. **No identifying column exists.** The schema has nowhere to put a name, so a
   careless write fails to compile rather than leaking.
2. **No raw response body is persisted.** The webhook receiver extracts ids,
   enums, amounts and assets and drops the rest — deliberately unlike the
   indexer's `indexed_events.payload` envelope. A delivery that fails to parse
   is recorded as a SHA-256 of the body, never the body.
3. **Handlers that display a name relay `serde_json::Value`** straight to the
   browser instead of binding a struct.

Onboarding follows from the same policy: customers are handed to Dakota's hosted
`application_url`, and beneficial owners, documents and SSNs never touch our
code.

Audit before merging anything that touches the schema:

```sh
grep -rniE '\b(name|email|ssn|dob|phone|address)\b' \
  rust-backend/services/dakota-service/src/db/migrations/*/up.sql
# expected: only wallets.address, a blockchain address
```

---

## Deferred: Sumsub import

Dakota sandbox does accept Sumsub **sandbox** share tokens — they are
environment-scoped (`sbx` vs `lv` prefix) and must be redeemed in the
environment that minted them. `POST /customers/bulk-import-sumsub-tokens` takes
1–100 tokens and always returns `200` with per-row `success`.

It is not self-serve, and two prerequisites are missing:

1. a **Dakota-issued partner token** for the sandbox environment — only from a
   Dakota representative, expires 30 days after creation;
2. the **"Share applicants data"** permission on our Sumsub app token.

Further limits: individual applicants only (business onboarding is explicitly
out of scope), Dakota redeems at the `id-only` verification level, and imported
applications land in **draft** missing employment status, SSN and attestations.
Completing those via API would mean handling SSNs, so the hosted form is the
only no-PII completion path.

KYC therefore ships hosted-redirect-only until someone chases the partner token.
