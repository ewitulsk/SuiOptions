# 08 — P4/P5: Power-user surface + marketplace

Lighter than P0–P3 by design: mostly composition of already-built parts. Written as work orders, not walkthroughs.

## P4.1 Bespoke-code pipeline (spec §7.4)

Flow is spec-first (D18): quiz → spec → **user approves** → code. Implementation:

1. **Sandbox authoring.** This is where opencode + Fly enters (deferred from P3 — 07 §2). agent-service gains `sandbox.rs`: create Fly app `cs-<env>-sbx-<session>` in a dedicated sandbox org, machine from a prebuilt `cs-sandbox` image (opencode + `curator-sdk` + docs, no network egress except OpenRouter via an allowlisting proxy sidecar or Fly machine egress policy — verify current Fly egress-control options at build time; fallback is a proxy container in the same app). Machine TTL minutes; destroyed on session close. The agent writes `strategy.py` implementing `curator_sdk.strategy.Strategy` for the *approved* spec version.
2. **Static gate** (test-runner gains `POST /v1/modules`): store module keyed by content hash; run `mypy --strict` (SDK's config) + ruff + an **import allowlist** AST check (`curator_sdk`, `math`, `statistics`, `decimal`, `dataclasses`, `typing` — no `socket`, `subprocess`, `os`, `ctypes`, dunder-import tricks). Failures return structured diagnostics into the agent loop.
3. **Simulation gate**: T1 standard + holdout and one T2 run keyed by `(spec_version, module_hash)` — the subprocess strategy host (06 §4) already runs arbitrary `Strategy` modules, which is why it was built instead of the Rust-primitives shortcut.
4. **Runtime**: provisioner builds a per-bot image (`registry.fly.io/cs-<env>-runtime-custom:<module_hash>`, `bot-runtime` base + the module baked in read-only) — bespoke code still runs under `runtime.py` scaffolding, no key material, gateway risk tier, on-chain limiter. Sandbox-depth decision (seccomp/egress hardening on the *runtime* machine) is spec open question #7 — resolve before GA of this path.
5. **Consent**: separate power-user acknowledgment recorded like deploy consent (provisioner `consents.kind = "bespoke"`).

## P4.2 Export & self-hosting (spec §10)

- **Export bundle** (provisioner `POST /v1/bots/:id/export`): tarball = template repo + spec + module (if bespoke) + README runbook; key blob from key-service `/internal/export` (03 §5). Delivery via one-time download URL; audit-logged.
- **Post-export**: key-service auto-revoke (D19) → gateway watermark-raise + bot stop + `bots.state = EXPORTED`; dashboard flips to "self-hosted: on-chain limits only".
- **SDK local-signing mode** (spec §9): `curator_sdk.signing.LocalSigner` (ed25519 over order digests — port `staging-mm-bot/src/signing.rs` byte-recipe to Python) + direct orderbook submission; PTBs for cancel-watermarks via a documented `sui client ptb` recipe or the gateway's build-only endpoint (`POST /v1/tx/build` returning tx bytes for local signing — add it here, not before).
- **Re-hosting**: provisioner flow = new key → user `rotate_curator` to the new curator address → new bot row. Already covered by 04's state machine with step 3–4 skipped.

## P4.3 BYO agent (spec §11)

- **MCP server**: small TypeScript package (`mcp-server/` top-level) wrapping the public APIs with user API keys: `get_spec/propose_spec/run_test/get_report/get_markets/docs/request_deploy`. `request_deploy` returns a dashboard URL — consent stays human (D20).
- **Skill**: markdown instruction layer over the same tools; ships in the template repo and as `.claude/skills/curator-studio`.
- **API keys**: provisioner issues `user_api_keys` (scoped: spec CRUD + tests only), metered by the same `test_meter`/`session_meter` plumbing.
- Parity holds by construction: the hosted agent already uses only these tools (07 §2).

## P4.4 T3 shadow mode (spec §13)

test-runner `t3_shadow.rs`: subscribe the paper bot to **production** orderbook WS; on each real fill event in its market, evaluate "would my resting shadow quote have won?" — fill iff shadow price beats the actual winning quote (we have the winner from `exchange_fills`). Needs read access to prod orderbook DB or a new paginated fills route (06 §2's note); prefer adding `GET /v1/markets/:m/fills?after=` to the orderbook service — useful beyond T3.

## P5 Marketplace (spec §12)

Hard gates first — these are entry conditions, not tasks to parallelize:
1. **Freeze the guard**: decide burn-upgrade-cap vs immutable republish (spec open Q3); existing-vault migration story documented; do it on staging first, then prod.
2. **Legal review** complete (spec open Q2).
3. **Live-data-only display** enforced in code review: marketplace queries touch only indexer + api-service + orderbook fills — no test-runner imports anywhere in the marketplace module tree (lint it: an import-boundary check in CI).

Build:
- **Listings**: provisioner `vault_listings (vault_id, curator_addr, listed_at, ack_id, delisted_at)`; curator opt-in from the dashboard with its own acknowledgment.
- **Track records**: computed from api-service `/trading-vaults/:id` + `/pps-history` + indexer events; cached daily rows `vault_track (vault_id, day, nav, pps, drawdown, tvl)`.
- **Discovery UI**: `/studio/market` — cards with strategy category (from spec primitive; bespoke labeled), live P&L/drawdown/TVL/age, hosting status, **guard policy summary** (band, cap, markets — the depositor's protection, surfaced as a feature).
- **Depositor journey**: vault detail → depositor-grade risk disclosure + consent → existing `vault::deposit` flow from their own wallet → depositor view (position, redeem — the always-available `request_withdraw` + fulfillment crank path; no operational controls).
- **Fees**: curator fee already exists on-chain (`curator_fee_bps` at `create_vault`, crystallized at fulfillment with the protocol cut to Treasury — `vault.move:708-719`). Marketplace platform take = the existing `protocol_fee_bps` knob; a separate listing fee needs new Move work — decide with monetization (spec open Q1).
- **Drills** (spec §19): depositor redeems with curator bot dead; owner revokes a malicious curator mid-listing; both on staging before GA.
