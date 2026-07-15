# twitter-service + social-bot: deployment prerequisites

Everything required to take `twitter-service` and `social-bot` from this repo
to running on the **staging** EC2 box. Both services are staging-only: they're
declared in `docker-compose.staging.yml` but not prod's, and their secrets
exist only under `options/staging/*` — `deploy.sh` skips them everywhere else.

Order matters. The deploy health-gates social-bot, and both services refuse to
boot on missing/placeholder secrets, so do the steps in this sequence.

---

## 1. External accounts (gather credentials)

### X / Twitter (per account you want to tweet from)

- An X developer account on the **pay-per-use plan** (prepaid credits;
  creating a post bills ~$0.01/request) with an app whose user authentication
  is set to **Read and Write**.
- From the developer portal, per account: **API Key**, **API Key Secret**,
  **Access Token**, **Access Token Secret** (OAuth 1.0a user context).
  Regenerate the access token *after* setting Read and Write — tokens minted
  before the permission change stay read-only.
- ⚠️ Known issue (mid-2026): some newly created pay-per-use apps get
  401/403 on `POST /2/tweets` with OAuth 1.0a due to an enrollment bug on
  X's side. Sanity-check your credentials with one curl before wiring
  everything up; if it fails, it's an X-support escalation (or we add
  OAuth 2.0 user-context support).

### Slack

- A Slack app (api.slack.com/apps → create, from scratch) in your workspace.
- Needed value: **Signing Secret** (Basic Information → App Credentials).
- The slash command and install steps happen *after* deploy (step 6) because
  the request URL must be live.

### Discord

- A Discord application (discord.com/developers/applications).
- Needed values: **Public Key** (General Information) and — only for the
  one-time command registration — the **Bot Token**.

---

## 2. Terraform (run once)

```sh
cd rust-backend/infra
terraform apply
```

This creates the new resources added for these services:

- ECR repos `options/twitter-service` and `options/social-bot` (`ecr.tf`) —
  the build workflow can't push without them.
- Secrets Manager placeholders `options/staging/twitter-service` and
  `options/staging/social-bot` (`secrets.tf`), both `REPLACE_ME` values with
  `ignore_changes` so your hand-set values never drift back.

No ALB/nginx-port/IAM changes are needed — nginx dispatches per-service, and
the GH-actions + EC2 roles already cover `options/*` repos and secrets.

## 3. Secrets (set real values — BEFORE the first deploy)

Fill both placeholders by hand. social-bot is health-gated by `deploy.sh` and
both services refuse to boot on `REPLACE_ME`, so a deploy with placeholder
secrets rolls back (noisily, by design).

```sh
# One entry per account under "accounts"; the key ("suioptions") is the
# account name used in /tweet <account> <text> and GET /accounts.
aws secretsmanager put-secret-value \
  --secret-id options/staging/twitter-service \
  --secret-string '{
    "accounts": {
      "suioptions": {
        "api_key":             "...",
        "api_key_secret":      "...",
        "access_token":        "...",
        "access_token_secret": "..."
      }
    }
  }'

aws secretsmanager put-secret-value \
  --secret-id options/staging/social-bot \
  --secret-string '{
    "slack_signing_secret": "...",
    "discord_public_key":   "..."
  }'
```

Adding a Twitter account later = add an entry to the JSON and redeploy (or
restart) twitter-service — no code change.

## 4. Config (allow list — who may tweet)

`services/social-bot/config/config.staging.toml` ships with **empty allow
lists, meaning nobody can post**. Add your team and land it like any code
change:

```toml
slack_allowed_user_ids   = ["U0123456789"]      # Slack: profile → ⋯ → Copy member ID
discord_allowed_user_ids = ["123456789012345678"] # Discord: dev mode → Copy User ID
```

Allow-listed users can post from **any** configured Twitter account.

## 5. Deploy (GitHub workflow)

Run the **Deploy staging** workflow (`deploy-lower.yml`, manual
`workflow_dispatch`) with **`force_all = true`**.

`force_all` is required for the *first* deploy that includes these services:
it seeds `TWITTER_SERVICE_TAG` / `SOCIAL_BOT_TAG` into the box's `.env`;
until every declared service has a tag, `deploy.sh` refuses partial deploys.
Subsequent deploys are normal — the changed-paths filter picks the services
up automatically.

The deploy health-checks social-bot via nginx
(`/staging/social-bot/health`) and rolls back on failure. twitter-service is
internal-only (port 9014, never proxied) and is not health-gated.

## 6. Platform webhooks (after the service is live)

Both platforms verify the endpoint when you save it, so this must come last.

**Slack** — Slash Commands → Create New Command:
- Command: `/tweet`, usage hint: `<account> <text>`
- Request URL: `https://<alb-host>/staging/social-bot/slack/command`
- Then: Install App to workspace.

**Discord**:
- General Information → **Interactions Endpoint URL**:
  `https://<alb-host>/staging/social-bot/discord/interactions`
  (Discord sends a signed PING on save — fails unless the deployed public
  key matches.)
- Register the `/tweet` command once (bot token needed only here; see
  `rust-backend/services/social-bot/README.md` for the exact curl).
- Install the app to your server (Guild Install).

## 7. Verify

```sh
# social-bot up (through nginx/ALB):
curl https://<alb-host>/staging/social-bot/health          # → ok

# twitter-service up (from the EC2 box; it's not public):
docker compose -f docker-compose.staging.yml exec social-bot \
  curl -s http://twitter-service:9014/accounts              # → ["suioptions"]
```

Then run `/tweet suioptions hello from staging` in Slack or Discord as an
allow-listed user. Failures surface in the channel reply, and twitter-service
failures also fire the `tweet-failed` Grafana alert.

---

## Quick checklist

- [ ] X pay-per-use app, Read+Write, OAuth 1.0a creds per account (curl-tested)
- [ ] Slack app created → signing secret in hand
- [ ] Discord app created → public key in hand
- [ ] `terraform apply` in `rust-backend/infra`
- [ ] `options/staging/twitter-service` secret set (real values)
- [ ] `options/staging/social-bot` secret set (real values)
- [ ] Allow lists filled in `config.staging.toml` and merged
- [ ] Deploy staging workflow run with `force_all=true`
- [ ] Slack slash command URL set + app installed
- [ ] Discord interactions URL set + `/tweet` registered + app installed
- [ ] `/tweet` end-to-end test posted
