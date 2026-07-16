# Marketplace Arbitrage Scraper — Implementation Plan

The idea: watch online marketplaces (eBay, Reverb, GunBroker, Craigslist, Facebook
Marketplace, …) for **new listings matching saved search parameters**, run each
listing through an **AI valuation step** that estimates true resale value, resale
speed, and risk, and **notify us immediately** when a listing's asking price is far
enough below its estimated resale value (the "$80 machinist tools worth $500" case).
Optionally, the AI drafts an outreach message to the seller — a human reviews and
sends it.

This lives in `scraper/` and is deliberately **self-contained** so the whole folder
(plus its `scraper-*.yml` workflows) can be lifted into any other repo. The parent
monorepo (`rust-backend/infra`, `.github/workflows/_deploy.yml`, `frontend/`) is
used as a *reference for conventions*, not as a dependency.

```
scraper/
├── PLAN.md                ← this file
├── README.md
├── backend/               ← Python scraper + AI valuation + API (FastAPI)
├── frontend/              ← Vite + React + TS dashboard (mirrors monorepo frontend stack)
└── infra/                 ← self-contained Terraform module + deployment bundle
.github/workflows/scraper-ci.yml       ← CI (path-filtered to scraper/**)
.github/workflows/scraper-deploy.yml   ← build → ECR → SSM deploy (modeled on _deploy.yml)
```

---

## 1. Backend — `scraper/backend/` (Python)

**Language choice: Python 3.12.** Not Rust — the scraping ecosystem is the decider:

- **Playwright** (headless Chromium, the strongest tool for JS-heavy/anti-bot pages),
  `httpx` + `curl_cffi` (TLS-fingerprint-impersonating HTTP for API-ish endpoints),
  `beautifulsoup4`/`selectolax` for parsing.
- **LiteLLM** gives us the OpenAI/Anthropic/OpenRouter plug-and-play requirement
  nearly for free (see §2).
- FastAPI + SQLAlchemy + Alembic for the API/persistence layer — boring and fast to build.

Tooling: `uv` for deps, `ruff` for lint/format, `pytest`, one `Dockerfile`
(multi-stage, installs Playwright + Chromium only in the worker image variant).

### Components (single deployable image, multiple compose services)

```
backend/app/
├── main.py           # FastAPI app (REST API for the frontend)
├── config.py         # pydantic-settings; env + config.toml
├── db/               # SQLAlchemy models + Alembic migrations
├── adapters/         # one module per marketplace, common interface
│   ├── base.py       #   MarketplaceAdapter: search(SavedSearch) -> list[RawListing]
│   ├── ebay.py       #   Tier 1: official Browse API (ToS-safe, reliable, has "newly listed" sort)
│   ├── reverb.py     #   Tier 1: official REST API
│   ├── gunbroker.py  #   Tier 1: official API (surfacing/valuation only — no transaction automation)
│   ├── craigslist.py #   Tier 2: HTML scrape (httpx + parser, no login needed)
│   └── fb_marketplace.py  # Tier 3: Playwright + session cookies (see Risks §6)
├── scheduler/        # poll loop: every saved search on its interval, fan out to adapters
├── pipeline/         # normalize → dedupe → persist → enqueue for valuation
├── valuation/        # the AI layer (see §2)
└── notify/           # Discord webhook first; Telegram/email later. Includes drafted
                      # outreach message in the alert — human copies/sends it.
```

**Normalized `Listing` model:** `source`, `external_id` (unique together), `url`,
`title`, `description`, `price`, `currency`, `location`, `photos[]`, `seller`,
`posted_at`, `scraped_at`. Adapters map marketplace payloads into this; everything
downstream (dedup, valuation, alerts, UI) is marketplace-agnostic — adding a new
marketplace is one adapter file + one row in the `sources` table.

**Data model (Postgres):**

| table            | purpose                                                                 |
|------------------|-------------------------------------------------------------------------|
| `sources`        | marketplace registry + adapter config (rate limits, credentials ref)    |
| `saved_searches` | query params per source (keywords, category, price cap, radius), poll interval, active flag |
| `listings`       | normalized listings, `UNIQUE(source, external_id)` for dedup             |
| `valuations`     | AI output per listing (versioned — re-valuation appends)                 |
| `alerts`         | fired notifications + their channel/status                               |
| `deals`          | manual lifecycle tracking: contacted → bought (cost) → sold (proceeds) → P&L |

`deals` matters beyond bookkeeping: it becomes ground truth for evaluating and
improving the valuation prompts/models (§2, eval harness).

**Flow per poll tick:**
scheduler → adapter.search() → normalize → skip already-seen → persist → valuation
pipeline → if `max_buy_price ≥ asking_price × threshold` → alert (Discord) + surface
in dashboard.

---

## 2. AI Valuation Layer — pluggable OpenAI / Claude / OpenRouter

**Abstraction: [LiteLLM](https://github.com/BerriAI/litellm) behind a thin internal
interface.** LiteLLM speaks all three providers (and ~everything else) through one
`completion()` call — switching provider is a config string, no code change:

```toml
# config.toml (or env overrides)
[valuation.triage]
model = "openai/gpt-5-mini"            # cheap, text-only pre-filter
[valuation.full]
model = "anthropic/claude-sonnet-5"    # vision + structured output
# or: "openrouter/deepseek/deepseek-chat", "openai/gpt-5", ...
```

We still wrap it in our own `Valuator` protocol (`valuate(listing) -> Valuation`)
so if LiteLLM ever becomes a liability we swap the internals without touching the
pipeline. API keys (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`)
come from Secrets Manager → env; only the configured provider's key needs to exist.

**Two-stage pipeline (cost control):**

1. **Triage** — cheap/fast model, title + price + category only. Kills the ~90% of
   listings that are obviously not deals. Pennies per thousand listings.
2. **Full valuation** — vision-capable model gets title, description, price,
   location, **and photos** (the micrometer example is exactly why photos are
   mandatory — the listing said "machinist tools", the photos said "Mitutoyo").
   Structured output (JSON schema / tool call), one `Valuation` record:

```json
{
  "est_resale_low": 400, "est_resale_high": 600,
  "expected_days_to_sell": 14,
  "max_buy_price": 250,
  "confidence": 0.72,
  "risk_flags": ["untested condition", "single blurry photo"],
  "resale_channel": "eBay sold-comps for Mitutoyo 293-series",
  "rationale": "...",
  "outreach_draft": "Hi, is the toolbox still available? ..."
}
```

**Eval harness (phase 5):** replay listings from `deals` (known buy/sell outcomes)
against candidate prompts/models; score estimate error. This is how we tune the
prompt and decide which provider is actually best — measured, not vibes.

---

## 3. Frontend — `scraper/frontend/`

Same stack as the monorepo frontend so it feels familiar: **Vite + React 18 +
TypeScript + TanStack Query + react-router**. No wallet/chain deps. Served as
static files by a Caddy container (which also terminates TLS — §4).

Pages:
1. **Deal feed** — new alerts, sorted by estimated margin; each card: photos,
   asking vs. est. resale range, risk flags, one-click "open listing" + copy
   outreach draft; actions: dismiss / mark contacted / mark bought.
2. **Saved searches** — CRUD for search params per marketplace, poll interval,
   alert threshold.
3. **Listing detail** — full valuation history, raw AI rationale, re-run valuation
   (with a different model, for comparison).
4. **Deals / P&L** — the bought→sold ledger and running profit.
5. **Settings** — provider/model selection per stage, notification channels.

Auth: single shared password → session cookie (this is a 2-person internal tool;
no user system). Backend enforces it on the API, Caddy can add basic-auth as a
second layer.

---

## 4. Infra — `scraper/infra/` (self-contained Terraform module)

A **distilled** version of `rust-backend/infra`: same patterns (single EC2 +
docker compose, ECR, Secrets Manager placeholders, GH-OIDC deploy role,
SSM-based deploys, cloud-init bootstrap via `templatefile`), minus what a
2-person bot doesn't need (ALB, RDS, multi-env prod/staging split, Tailscale,
full Loki/Grafana/Tempo stack).

```
infra/
├── versions.tf        # aws ~> 5.70, random — same pins as monorepo
├── variables.tf       # project="scraper", region, github_repo (owner/repo — the ONLY
│                      # thing to change when moving repos), deploy branches, instance
│                      # type (t3a.small), domain_name + route53_zone_id (optional)
├── vpc.tf             # minimal: default-VPC data source OR a tiny public-subnet VPC
├── ec2.tf             # one host; cloud-init installs docker, lays out /opt/scraper,
│                      # drops compose + deploy.sh + render-secrets.sh (templatefile,
│                      # same mechanism as monorepo ec2.tf)
├── ecr.tf             # for_each over ["backend", "frontend"], keep-last-20 lifecycle
├── iam.tf             # GH Actions OIDC provider + deploy role (ECR push, SSM send,
│                      # S3 bundle bucket), EC2 instance profile (ECR pull, secrets read, SSM)
├── secrets.tf         # placeholders: scraper/llm (openai/anthropic/openrouter keys),
│                      # scraper/db (random_password), scraper/notify (discord webhook),
│                      # scraper/app (session secret, marketplace API keys, proxy creds)
├── security_groups.tf # 80/443 in, egress open
├── dns.tf             # optional Route53 A record (skip if zone id empty — same pattern
│                      # as monorepo variables.tf)
├── outputs.tf         # ecr registry, instance id, deploy role arn — paste into GH repo vars
├── templates/cloud-init.sh.tftpl
└── deployment/
    ├── bake.hcl                 # buildx targets: backend, frontend (gha cache scopes,
    │                            # linux/amd64 — same conventions as monorepo bake.hcl)
    ├── docker-compose.yml       # postgres:16 (volume-backed), backend-api, backend-worker
    │                            # (scheduler+scrapers, same image, different command),
    │                            # caddy (TLS via Let's Encrypt, serves frontend static
    │                            # build, reverse-proxies /api → backend)
    ├── Caddyfile
    ├── deploy.sh                # pull images at $IMAGE_TAG, render secrets → env files,
    │                            # compose up -d, health check
    └── render-secrets.sh        # Secrets Manager JSON → env files; refuse to start on
                                 # missing/malformed secret (fail-noisy, like monorepo)
```

Deliberate simplifications vs. the monorepo (each has an upgrade path if the bot
earns it):
- **Postgres in compose** (EBS volume) instead of RDS — a `use_rds` flag can add it later.
- **Caddy** instead of ALB + ACM — free TLS, one container, no cert plumbing.
- **Single env** — `environment` variable defaults to `prod`; a second env is a
  second `terraform workspace` + compose dir, not new code.
- **Playwright needs headroom**: t3a.small minimum, t3a.medium if FB Marketplace
  (Tier 3) is enabled.

**Portability contract:** nothing in `scraper/infra` references files outside
`scraper/`. Moving repos = copy `scraper/` + two workflow files, run
`terraform apply` with the new `github_repo` var, set three GH repo variables
(`AWS_REGION`, `ECR_REGISTRY`, `DEPLOY_ROLE_ARN`) from terraform outputs.

---

## 5. GitHub Workflows

Path-filtered to `scraper/**` so they coexist with the monorepo's pipelines and
copy cleanly to a standalone repo (where the filter simply always matches).

**`scraper-ci.yml`** — PRs + pushes touching `scraper/**`:
- backend: `uv sync` → `ruff check` → `pytest` (adapter tests run against recorded
  HTTP fixtures — VCR-style — so CI never hits live marketplaces)
- frontend: `npm ci` → `tsc -b --noEmit` → `vite build`
- infra: `terraform fmt -check` + `terraform validate`

**`scraper-deploy.yml`** — push to `main` with `scraper/**` changes, plus
`workflow_dispatch` with an `image_tag` input for rollbacks (same rollback
semantics as `_deploy.yml`):
1. **build** — matrix over `[backend, frontend]`, `docker buildx bake --push`
   against `infra/deployment/bake.hcl`, tag = short SHA. (Only two images — no
   `affected.py` equivalent needed; a simple `paths` filter per target is enough.)
2. **deploy** — OIDC assume role → tar the deploy bundle (compose + deploy.sh +
   render-secrets.sh + Caddyfile) to S3 → `aws ssm send-command` on the EC2 host:
   download bundle, `./deploy.sh $TAG` → poll SSM invocation to terminal state
   (long-poll loop, not `ssm wait`, per the monorepo's learned lesson in
   `_deploy.yml`).

---

## 6. Risks & ground rules

- **Prefer official APIs.** eBay (Browse API), Reverb, and GunBroker all have
  real APIs — Tier 1 sources are ToS-clean and stable. Craigslist HTML is
  low-drama. **Facebook Marketplace has no API and scraping it violates FB ToS**
  (account bans, blocks; needs logged-in Playwright sessions and possibly
  residential proxies). It's scoped as Tier 3 / last, behind a config flag, eyes
  open. Budget line item for a proxy service if/when we turn it on.
- **Outreach stays human-in-the-loop.** The AI *drafts* the message; a person
  sends it. Auto-messaging sellers is both a ToS violation on most platforms and
  a good way to torch accounts. Revisit only per-platform where it's allowed.
- **GunBroker = firearms**: the bot only *surfaces and values* listings there;
  purchase/transfer stays fully manual and through the normal FFL process.
- **LLM spend**: the two-stage triage design caps cost; add a per-day token
  budget + kill switch in config from day one.
- **Anti-bot drift**: scrapers rot. Adapter health metrics (last success, empty-result
  streaks) surface in the dashboard + a Discord "adapter broken" alert.

---

## 7. Build order (each phase ends verifiable)

| Phase | Scope | Verify |
|-------|-------|--------|
| **1. Vertical slice** | backend skeleton (FastAPI, DB, config), eBay adapter, scheduler, LiteLLM valuator (triage+full), Discord alert. Runs via local `docker compose up`. | A saved search finds a real underpriced eBay listing and posts a valued alert to Discord. |
| **2. Dashboard** | frontend (deal feed, saved-search CRUD, listing detail), auth, deals/P&L tables. | Manage searches + review deals entirely from the browser. |
| **3. Ship it** | `scraper/infra` terraform, workflows, first cloud deploy. | Push to `main` → CI green → auto-deploy → bot runs 24/7, alerts arrive. |
| **4. More sources** | Reverb + GunBroker adapters (API), Craigslist (HTML). Adapter health monitoring. | Each new source produces deduped, valued listings for a week without manual poking. |
| **5. Sharpen the AI** | eval harness on `deals` outcomes, prompt/model comparison in UI, outreach drafts, per-category prompt tuning. | Valuation error measured and trending down against real sold outcomes. |
| **6. (Optional) Tier 3** | FB Marketplace via Playwright + proxies, behind config flag. | Stable for 2 weeks without account/block incidents. |

Phase 1 is the whole thesis compressed: if the valuations on real eBay data aren't
good enough to act on, we find out in week one before building anything else.
