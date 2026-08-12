# 07 — P3: Dashboard + `agent-service`

## 1. Where the dashboard lives

**A new standalone app in its own folder: `studio-dashboard/`, built on the same stack as `frontend/`** — Vite 5 + React 18 + TypeScript + react-router 7 + `@mysten/dapp-kit` + TanStack Query + PostHog. `exchange-dashboard/` is the in-repo precedent for exactly this shape (own folder, own `package.json`/`vercel.json`, own Vercel project rooted at the folder); mirror its setup.

What it borrows vs. owns:

- **Copy, don't share (initially):** the sponsored-PTB client pattern from `frontend/src/tx/*.ts` (studio's builders live in `studio-dashboard/src/tx/` — the gas-station template doc-comments cross-reference these files), plus the small formatting/token-catalog helpers. If drift between the three apps starts hurting, extract a shared workspace package then — not now.
- **Own:** routing, screens, state. No coupling to the trading app's release cadence — studio ships on its own Vercel project.
- **Deploy:** new Vercel project rooted at `studio-dashboard/`, same branch discipline as the others (staging pushes = previews; prod = ff-push `main`); set env vars via the Vercel REST API (the CLI's env-add-preview path is buggy). PostHog wiring copies `frontend/`'s pattern (`/ph` proxy, `capture_exceptions`, hidden sourcemaps script).

Routes (react-router):

```
/studio                     bot list (cards: state chip, heartbeat freshness, P&L spark, TVL)
/studio/new                 quiz (agent session UI)
/studio/spec/[id]           spec view: human-readable + raw TOML/JSON, version history, reports
/studio/bot/[vaultId]       operate: status, positions, greeks, guard-rejection feed,
                            pause/resume/KILL, run test, reopen agent, export (P4), revoke
/studio/bot/[vaultId]/records   consents, reports, signing audit (read-only)
```

Data sources: provisioner (`/v1/bots`, `/v1/specs`, `/v1/consents`), bot-gateway admin routes + WS (live status), test-runner (`/v1/reports/:id`), api-service (`/trading-vaults/:id` for vault economics), indexer GraphQL (guard events — `bounded_curator` events arrive once added to the indexer's `event_types.rs`; add `GuardCreated`, `GuardRevoked`, `PolicyRejected`, `SignerAdded` in the P0 PR so history exists by P3).

User-signed transactions (all sponsored → templates from 04 §6): deposit into vault (existing flow), `rotate_curator` (the REVOKE button — red, confirm-gated), `unwrap` (P4 export), `set_policy`.

The KILL button calls provisioner `POST /v1/bots/:id/kill` → gateway control push + watermark raise + optional Fly machine stop. It must not depend on the bot's cooperation (spec D16) — the UI reflects gateway-confirmed state, not bot acks.

## 2. `agent-service`

New crate `rust-backend/services/agent-service`, port **9025**, nginx-routed (dashboard talks to it; it talks to OpenRouter and the sandbox fleet). It is deliberately a *broker*, not the agent itself: sessions are stateless sandboxes rehydrated from DB (spec §8.5).

```
services/agent-service/
  src/{main,lib,config,router,state}.rs
  src/handlers/{sessions,messages,tools}.rs
  src/{sandbox.rs,openrouter.rs,tools_impl.rs,prompt.rs}
  src/db/…            sessions, messages, session_meter
```

### Sessions

```
POST /v1/sessions               { walletAddr } → { sessionId }        (quiz start)
POST /v1/sessions/:id/messages  { text } → SSE stream of agent turns
POST /v1/sessions/:id/attach-report { reportId }                      ("send results to agent")
GET  /v1/sessions/:id           transcript + current spec ref
```

State = `sessions (id, user, vault_id?, spec_id?, status)` + `messages (session_id, seq, role, body)` + current spec pointer. A session survives weeks; each user turn rehydrates context (transcript tail + spec + attached reports) into a fresh model call. **v1 implementation is direct OpenRouter chat-completion calls with tool-calling** (`deepseek/deepseek-v4-flash-0731` — hold the exact slug in config, verify against OpenRouter's catalog at build time) — the opencode-in-sandbox machinery is only required when the agent must *write code*, i.e. P4's bespoke path. Don't build sandbox orchestration before P4 needs it; the spec's "ephemeral build sandbox" requirement is satisfied trivially by there being no code execution at all in v1. (Record this as an accepted simplification of spec §8.5.)

### Tools (the hard-scoped surface, spec §17)

`tools_impl.rs` implements exactly:

```
inspect_wallet(addr)      → token balances via ChainClient::all_balances + token catalog
get_spec / propose_spec   → provisioner spec CRUD (agent proposes; version bump on accept)
run_test(mode, window)    → test-runner POST /v1/tests (metered)
get_report(id)            → test-runner report JSON
get_markets()             → orderbook /v1/markets (via gateway cache)
docs(query)               → static primitive-library docs bundled at build
```

No deploy tool: deploy/consent is dashboard-only (spec D20 applies to *all* agents, hosted included — cleanest to have no exception).

### Prompt (`prompt.rs`)

System prompt encodes: the canned quiz dimensions (spec §4.2) in order; the rule that every `[risk]` field must trace to a user answer; the overfitting warning it must voice when a user iterates on backtests (spec D14); the AI-provenance disclosure it must repeat before proposing deploy; refusal of anything outside strategy construction (abuse control). Keep the prompt in-repo and versioned; log `prompt_version` on every session.

### Metering (spec §17)

`session_meter (user, day, tokens_in, tokens_out, requests)` updated from OpenRouter usage fields; hard daily cap in config → 429 with a friendly dashboard message. Per-session request cap too (runaway-loop guard).

## 3. Consent flow (dashboard-side)

The deploy page renders: spec vN (human-readable), report R on vN (holdout revealed here — `final: true` test), the acknowledgment text (typed phrase, not checkbox). Submitting calls provisioner `POST /v1/consents` then `POST /v1/deploys`. The consent row stores the *text hash* + spec hash + report id (spec D3); the UI blocks deploy when the current spec version has no report (spec D13 hard rule).

## 4. P3 exit drills (spec §19 gate)

- Retail tester: quiz → spec → test drive → consent → deploy → fund → live, unassisted, on staging.
- Kill drill: wedge a bot (pause its loop via test hook) → dashboard KILL → orders watermarked + machine stopped within 60s.
- Revoke drill: user wallet `rotate_curator` → bot's next guarded call aborts → dashboard shows REVOKED within one poll cycle.
- Meter drill: exhaust a test user's daily agent budget → clean 429 UX, no session corruption.
