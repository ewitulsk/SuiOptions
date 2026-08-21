# Leaderboard + Sui Event-Ingestor (go-backend) — Build Plan

## Context

We want a points/leaderboard system: users (accounts = any combination of wallet / twitter / discord identities) accrue points, driven by configurable on-chain Sui events. Two new microservices, written in **Go** in a new top-level `go-backend/` monorepo (first non-Rust backend code in the repo):

1. **leaderboard** — identity-agnostic accounts + points ledger; internal write API (increment/decrement, link/merge identities); public read API (ranked list, rank+neighbors, per-account breakdown, sources).
2. **event-ingestor** — DB-driven (no config-file) event→points rules. Admin adds a Sui package; the service introspects its modules/event structs from chain; admin configures per-event rules (points, recipient = tx sender or an event field, enabled, start = "now" or a timestamp with best-effort backfill). Service polls Sui GraphQL per module with persisted cursors and posts points to leaderboard idempotently.

Plus frontend: a public **Leaderboard tab** (filters: time range, wallet search, points source; pinned "your position + neighbors" card; per-account points breakdown) and an **Event Ingestor admin section** on `/admin` (JWT-gated).

Key constraints discovered in exploration:
- The in-house Rust indexer is **compile-time whitelisted** (`services/indexer/src/event_types.rs`) — it cannot ingest arbitrary packages. The ingestor therefore polls **Sui GraphQL directly**, mirroring the proven pattern in `rust-backend/services/orderbook/src/sync.rs` (per-module streams + cursor table) and `rust-backend/crates/sui-tx/src/events.rs` (the exact GraphQL events query, page cap 50).
- JSON-RPC is dead on Sui fullnodes (July 2026). Package introspection uses GraphQL `object(address){asMovePackage{module{structs…}}}` (answer to "is this possible?": **yes**).
- `deployment/affected.py` hard-requires a `Cargo.toml` per service (`crate_globs()` + `test_affected.py::test_every_service_has_a_manifest`) — must be taught about Go services or CI breaks / every commit rebuilds the fleet.
- Product decisions: per-rule configurable start (tip or timestamp, best-effort backfill given pruned public RPC history); filters = time range + wallet search + source; plus per-user breakdown of which events earned their points.

---

## Part 1 — `go-backend/` monorepo scaffolding

Single Go module `github.com/ewitulsk/SuiOptions/go-backend`, Go 1.24. Stdlib `net/http.ServeMux` (1.22 method+wildcard routing — no chi), `jackc/pgx/v5` + `pgxpool`, migrations via `pressly/goose/v3` embedded with `embed.FS` and run at boot (Go analogue of diesel `embed_migrations!`), `BurntSushi/toml` config.

```
go-backend/
├── go.mod / go.sum
├── Dockerfile.leaderboard / Dockerfile.event-ingestor
├── cmd/
│   ├── leaderboard/{main.go, config/config.{toml,staging.toml,prod.toml}}
│   └── event-ingestor/{main.go, config/config.{toml,staging.toml,prod.toml}}
└── internal/
    ├── platform/
    │   ├── config/       # LoadTOML: ${VAR} env expansion, fail-fast on missing (mirrors runtime_config)
    │   ├── db/           # pgxpool connect + goose.Up(embed.FS)
    │   ├── authclient/   # POST http://auth-service:9008/verify; RequireAuth middleware:
    │   │                 #   401 invalid/missing, 502 fail-closed on transport error; address → ctx
    │   ├── obs/          # otel via OTEL_EXPORTER_OTLP_ENDPOINT (no-op unset), promhttp /metrics,
    │   │                 #   /health returning literal body "ok" (gatus asserts [BODY] == ok)
    │   ├── suiaddr/      # Normalize(addr) — port of auth-service allowlist.rs (lowercase, 0x, pad 64);
    │   │                 #   CanonicalType comparison (0x-padded vs bare TypeName trap)
    │   └── suigraphql/   # GraphQL POST client; events query (asc/desc) ported from sui-tx events.rs;
    │                     #   package introspection queries
    ├── leaderboard/      # config, server (public+internal mux), api_public, api_internal,
    │   └── store/        #   service (merge algorithm), all SQL + migrations/00001_init.sql
    └── eventingestor/    # config, server, api_admin, introspect, extract, poller, backfill,
        └── store/        #   lbclient (leaderboard internal client), migrations/00001_init.sql
```

Config mirrors the Rust convention: per-env TOML selected by `APP_ENV` in the Docker ENTRYPOINT; only `APP_ENV`, `LOG_LEVEL`, `OTEL_EXPORTER_OTLP_ENDPOINT`, `DB_PASSWORD`, `DB_HOST` come from env. No secrets mount needed v1 (public GraphQL URL lives in TOML; no keys).

**Ports** (verified free; 9020/9030 are nginx host ports — avoided): leaderboard **9021 public / 9022 internal**; event-ingestor **9023 admin-public / 9024 internal**. Internal ports never nginx-routed (token-info 9005/9006 convention).

## Part 2 — leaderboard service

**DB `leaderboard_<env>`** (DDL summary):
- `accounts (id, created_at, merged_into NULL)` — `merged_into` is audit-only; merge repoints all rows so queries never chase chains.
- `account_identities (account_id, identity_type CHECK IN ('wallet','twitter','discord'), identifier, UNIQUE(identity_type, identifier))` — wallet identifiers normalized via `suiaddr.Normalize`; twitter = lowercase handle. Identity-agnostic: new types = extend the CHECK.
- `points_entries (account_id, delta BIGINT signed, source TEXT e.g. 'rule:42'|'admin:manual', event_type TEXT NULL, idempotency_key TEXT UNIQUE NULL, occurred_at TIMESTAMPTZ, created_at)` + indexes on (occurred_at), (account_id, occurred_at), (source, occurred_at), (event_type, occurred_at).
- `account_totals (account_id PK, total, updated_at)` — cached all-time totals, maintained in the same tx as every insert; index (total DESC).
- `sources (source PK, event_type, label)` — upserted from the optional `source_label` on internal writes; feeds the public filter dropdown with human labels.
- `account_merges (winner_account_id, loser_account_id, merged_at)`.

**Merge** (single tx in `service.go`): `pg_advisory_xact_lock` on both ids ascending (no deadlock); winner = lower id; repoint identities + entries, fold totals, mark loser `merged_into`, record merge. Idempotency keys survive merges because the UNIQUE constraint is global.

**Ranking**: all-time = `RANK() OVER (ORDER BY total DESC)` on `account_totals`; windowed (30d/7d/24h) or source-filtered = `SUM(delta)` over `points_entries` in a CTE, ranked; neighbors = same CTE filtered `rank BETWEEN target±radius`.

**Endpoints — internal `:9022`** (compose-network only, unauthenticated per existing internal-port trust model):
- `POST /internal/points` `{identity:{type,identifier}, delta, source, source_label?, event_type?, idempotency_key?, occurred_at?}` → auto-creates account+identity; duplicate key → `200 {applied:false}` (idempotent success). Negative delta = removal.
- `POST /internal/link` `{a:{type,identifier}, b:{type,identifier}}` → 4 cases: neither exists → one account with both; one exists → attach; both on different accounts → **merge**; same account → no-op. Returns `{account_id, merged}`.
- `GET /health`, `GET /metrics`.

**Endpoints — public `:9021`** (nginx `/{env}/leaderboard/…`, read-only):
- `GET /leaderboard?window=all|30d|7d|24h&source=&limit<=100&offset=` → `{window, source, as_of_ms, total_accounts, limit, offset, entries:[{rank, account_id, wallets:[..], twitter|null, points, event_count}]}`
- `GET /rank/{wallet}?window=&source=&radius=` (default 5, max 25) → `{rank, points, account_id, wallets, neighbors:[entries incl. target], total_accounts}`; 404 unknown/no points in window.
- `GET /account/{wallet}/breakdown?window=` → `{account_id, total, by_source:[{source, label, event_type, points, event_count, last_event_ms}]}`
- `GET /sources` → `{sources:[{source, label, event_type}]}`
- `GET /health`

## Part 3 — event-ingestor service

**DB `event_ingestor_<env>`**:
- `tracked_packages (id, package_address UNIQUE normalized, label, modules_json JSONB, introspected_at, created_by, created_at)` — `modules_json` caches the chain introspection (modules → candidate event structs → fields) for the admin UI; refresh by delete+re-add in v1.
- `event_rules (id, package_address, module_name, event_type UNIQUE-per-module canonical '0x<64>::mod::Struct', label, points BIGINT, recipient_mode CHECK('sender','field'), recipient_field TEXT NULL, start_mode CHECK('tip','timestamp'), start_at NULL, backfill_state CHECK('none','pending','running','done','exhausted'), backfill_cursor, enabled, created_by, timestamps)`.
- `module_cursors (package_address, module_name) PK, cursor TEXT "{package}|{opaque_cursor}", updated_at` — the `pkg|cursor` self-heal pattern from price-charting `exchange_watcher.rs`.
- `deliveries (rule_id, idempotency_key UNIQUE, recipient, points, event_time, delivered_at)` — audit + re-POST skip; leaderboard's idempotency key remains the dedupe authority.

**Admin API `:9023`** (nginx `/{env}/ingestor/…`; everything except `/health` behind `RequireAuth` → auth-service `/verify`; `created_by` = verified address):
- `POST /packages {package_address}` → synchronous chain introspection; 201 with the full package object (modules + candidate events + fields); 400 unknown/eventless.
- `GET /packages` → tracked packages **with introspection embedded** (small payloads; keeps admin UI to 3 queries).
- `DELETE /packages/{package_address}` → 204, cascades rules/cursors (confirm in UI).
- `POST /rules`, `GET /rules`, `PATCH /rules/{id}` (points/enabled/recipient/label), `DELETE /rules/{id}`. Validation: canonical event_type exists in the package's introspection; `recipient_field` required iff mode=field; `start_at` required iff start_mode=timestamp (sets `backfill_state='pending'`).
- `GET /status` → per module stream: `{package_address, module, cursor_updated_at, last_event_ms, lag_ms}` + per-rule `{backfill_state, delivered, last_delivery_at}`.
- `GET /health` (unauth). Internal `:9024`: `/health`, `/metrics` (poll lag, deliveries, GraphQL error counters, backfill progress).

**Introspection** (`introspect.go`) — GraphQL against `sui_graphql_url`:
`object(address){asMovePackage{modules(first:50){nodes{name}}}}` then per module `module(name){structs(first:50){nodes{name abilities fields{name type{repr}}}}}` (paginate both). **Candidate-event heuristic**: abilities include COPY+DROP, exclude KEY (the `event::emit` bound); admin confirms — GraphQL has no "is event" marker. Verify live schema field names in Phase 1 (only the events query is battle-tested in-repo; `asMovePackage` is new ground).

**Forward poller** (`poller.go`) — one goroutine per module having ≥1 enabled rule (supervisor restarts w/ backoff); mirrors `orderbook/src/sync.rs`:
1. Load cursor; if package half ≠ current package or absent → seed at tip (descending `last:1`, take `startCursor`).
2. Every `poll_interval_ms` (2000): ascending pages, `filter:{module:"pkg::mod"}`, `first:50` (server cap), loop while `hasNextPage`. Exact query ported from `sui-tx/src/events.rs` (`nodes{ sequenceNumber timestamp transactionModule{name} sender{address} transaction{digest} contents{type{repr} json} }`).
3. Per node (malformed → skip, never fatal): canonical-match `contents.type.repr` against enabled rules; **start gating** — timestamp rules skip events older than `start_at`, tip rules skip events older than rule `created_at` (module streams are shared); extract recipient; POST to leaderboard with `idempotency_key = "{tx_digest}:{event_seq}:{rule_id}"`, `source = "rule:{id}"`, `source_label = rule.label`, `occurred_at` = event ts; retry 5xx/transport with capped backoff; record delivery `ON CONFLICT DO NOTHING`.
4. **Persist cursor only after the whole page is delivered** — crash mid-page replays the page; leaderboard idempotency dedupes.
5. Rules/modules re-read from DB each tick → admin changes live without restart.

**Recipient extraction** (`extract.go`) — mode `sender` → GraphQL `sender.address`; mode `field` → dotted path over `contents.json`, honoring the rendering traps from `docs/sui-json-rpc-migration.md`: nested structs unwrapped (plain keys), `Balance/Supply` → `{value}` hop written by admin, enum `@variant` segments allowed, UID/ID = bare string leaf, address leaves may lack `0x` → validate 1–64 hex then `suiaddr.Normalize`; base64 vectors rejected. Unresolvable → metric + log + skip (no cursor stall).

**Backfill** (`backfill.go`) — one worker over `pending|running` rules: descending walk (`last/before`, page-reversal + `startCursor` continuation as in events.rs), same delivery pipeline (idempotency covers overlap at the seed point), persist `backfill_cursor` per page, throttled (`backfill_pages_per_sec` default 2). Stop `done` when a page's oldest event < `start_at`; stop `exhausted` when history ends first (public RPC prunes — surfaced in `/status`).

## Part 4 — Deployment touchpoints (all must land)

1. `go-backend/Dockerfile.{leaderboard,event-ingestor}` — golang:1.24-bookworm builder, `CGO_ENABLED=0`, debian:bookworm-slim runtime **with curl + ca-certificates** (compose healthcheck), config copied, `ENTRYPOINT … --config /app/config/config.${APP_ENV}.toml`.
2. `rust-backend/deployment/bake.hcl` — two targets NOT inheriting `_common` (bake runs with `working-directory: rust-backend`): `context = "../go-backend"`, own gha cache scopes; add to `group "default"`.
3. `rust-backend/deployment/affected.py` — **the trap**: add `GO_SERVICE_GLOBS = {"leaderboard": ["go-backend/**"], "event-ingestor": ["go-backend/**"]}`, add both to `ALL_SERVICES`, keep `crate_globs()` iterating Rust `SERVICE_GLOBS` only. Update `test_affected.py` (manifest test → Rust services only; new tests: go-backend change selects both Go services, Rust crate change doesn't; ALL_SERVICES↔deploy.sh sync). Runs in `deploy-filter-ci.yml`.
4. `rust-backend/deployment/ec2/deploy.sh` — `ALL_SERVICES`, `tag_var_for` (`LEADERBOARD_TAG`/`EVENT_INGESTOR_TAG`), `compose_name_for` (identity), `health_path_for` (`/$ENV/leaderboard/health`, `/$ENV/ingestor/health`).
5. `.github/workflows/_deploy.yml` ~line 162 — append both to the hardcoded force_all JSON.
6. `docker-compose.staging.yml` **and** `.prod.yml` — `image: ${ECR}/options/<name>:${<NAME>_TAG}`, env `APP_ENV`, `DB_PASSWORD`, `DB_HOST`, `OTEL_EXPORTER_OTLP_ENDPOINT`; ingestor `depends_on: [leaderboard, auth-service]`; network `net`; no secrets mount.
7. `nginx.staging.conf` **and** `.prod.conf` — `location ~ ^/staging/leaderboard(?:/(?<tail>.*))?$ { set $upstream leaderboard:9021; … }` and `/staging/ingestor` → `event-ingestor:9023`; 9022/9024 never routed.
8. `rust-backend/infra/ecr.tf` — add both to `local.service_repos`; **`terraform apply` before first image push** (options-2 worktree state discipline applies).
9. `wipe-provision-db.sh` DB_PREFIX cases (`leaderboard`, `event_ingestor`) + `wipe-provision-db.yml` choice list.
10. Monitoring — `prometheus.yml` scrape `leaderboard:9022`, `event-ingestor:9024`; `gatus-config.yml` two `/health` endpoints, `[BODY] == ok`.
11. `start-service.yml` / `stop-service.yml` (+ `stop-services.yml` if it enumerates) choice lists.
12. **New `.github/workflows/go-ci.yml`** — on `go-backend/**` PRs: `gofmt -l`, `go vet ./...`, `go test ./...` with a postgres:16 service container (`TEST_DATABASE_URL`-gated integration tests).
13. First deploy must be `force_all=true` (tag seeding). Note: the deployment PR touches `REBUILD_ALL_GLOBS` paths → full fleet rebuild on merge (expected).

## Part 5 — Frontend

**New files**: `src/api/leaderboard.ts` (DTOs + fetch + typed errors + hooks, single-file style of `src/api/analytics.ts`), `src/api/ingestorAdmin.ts` (JWT client + hooks, style of `src/api/tokenAdmin.ts`), `src/screens/Leaderboard.tsx`, `src/components/EventIngestorManager.tsx`.
**Modified**: `src/config.ts` (`LEADERBOARD_URL` ← `VITE_LEADERBOARD_URL` default `http://127.0.0.1:9021`; `INGESTOR_URL` ← `VITE_INGESTOR_URL` default `http://127.0.0.1:9023`), `src/App.tsx` (route before `*`), `src/components/Header.tsx` (tab after Analytics; public tab → **no** pill dep-array change needed), `src/screens/Admin.tsx` (mount `<EventIngestorManager flash={flash} />` after TokenManager at line 332), `src/styles/aqua.css` (one appended `lb-*` block).

**Leaderboard screen** (`useCurrentAccount()` for connected wallet):
- State: `window` (all/30d/7d/24h), `source`, `offset` (PAGE=50, reset on filter change), committed `searchAddress`, `expanded` (accordion).
- Hooks: `useLeaderboard` (key `["leaderboard", window, source, limit, offset]`, `placeholderData: keepPreviousData`, staleTime 30s), `useLeaderboardSources`, `useLeaderboardRank(address|null, window)` (enabled-gated), `useLeaderboardBreakdown` (fetched lazily inside the expansion panel).
- Sub-components (in-file): `RangePills` (copy of Analytics `RangeFilter` / `useSegmentPill`), `SourceSelect` (native select from `/sources`), `SearchBox` (validate `/^0x[0-9a-fA-F]{1,64}$/`, Enter to commit, × to clear), `PositionCard` (pinned card — rendered for the connected wallet always, and for search results; big rank + points + "top X%", mini neighbors table with `.lb-row--you`, "Go to page N" sets offset; **neighbors live in the pinned card, never spliced into the paged table**), `LeaderboardTable` (`.vault-table` + mobile `--cards` variant; Rank · Account (`<Address>`) · Points · Events + caret; row click expands `BreakdownPanel`), `BreakdownPanel` (per-source rows + total), `Pager`.
- States: WaveLoader initial; 503 → friendly `dash-alert`; empty → `dash-empty`; no wallet → hint line.

**EventIngestorManager** (structural copy of `TokenManager.tsx`: own `useAdminAuth()`, sign-in row, `runAuthed` with `AuthExpiredError → signOut`, busy-string, `window.confirm`, local `Field` copy — a third duplicate matches convention):
1. **Tracked packages** — admin-table (Package chip · modules · event types · Configure/Remove); add form posts `POST /packages`, auto-selects on success.
2. **Points rules** — table of every candidate event across the selected package's modules (rule summary "10 pts → sender" / "5 pts → field `writer`" / "no rule" tag); inline RuleForm: label, points, recipient select (sender vs field → second select of the struct's fields, address-typed first), enabled checkbox, start radio ("From now" / "Backfill from timestamp" + `datetime-local`).
3. **Ingestion status** — module streams + backfill states, freshness tags from `lag_ms`, `refetchInterval: 15_000`.
- Query keys `["ingestor-packages"|"ingestor-rules"|"ingestor-status"]`; rule mutations also invalidate public `["leaderboard"]`-prefixed keys.

CSS: `.lb-controls`, `.lb-select`/`.lb-search__input` (admin-field__input recipe), `.lb-pos*` (vault-card look), `.lb-row--you`, `.lb-rank--top`, `.lb-breakdown*`, `.lb-pager*`. Admin section needs no new classes.

## Phases & verification

1. **Platform scaffolding** (`go.mod`, `internal/platform/*`, go-ci.yml). Verify: `go build/vet/test ./...`; **live-probe the introspection queries** against `https://graphql.testnet.sui.io/graphql` with the exchange package to confirm 2026 schema field names (gate for `pkg.go`).
2. **Leaderboard service.** Verify: local Postgres + curl script — points → link → merge combines totals → idempotency replay `applied:false` → window/source ranks → neighbors edges → breakdown sums; `/health` == `ok`.
3. **Ingestor admin plane** (schema, introspection, CRUD behind RequireAuth). Verify: local/fake auth-service; add exchange package; candidate events listed with fields; 401/502 paths.
4. **Ingestion** (poller, extract, lbclient, backfill). Verify: live testnet run watching exchange `settlement`; points appear in leaderboard; `kill -9` mid-page → no double count; timestamp rule reaches `done`/`exhausted`.
5. **Frontend public path** (config, api client, screen, route, tab, CSS) then **position/search/breakdown** then **admin section**. Verify: `npm run build`; dev against local services (or a canned-JSON mock); eyeball paging without flash, pinned position card for a page-N wallet, lazy breakdown fetches, mobile cards at <640px; admin flow end-to-end incl. expired-JWT flash.
6. **Deployment plumbing** (Part 4). Verify: `python3 -m unittest discover -s rust-backend/deployment` green; local `docker buildx bake` of both targets; `terraform plan` shows exactly two ECR repos.
7. **Staging rollout**: terraform apply → wipe-provision-db for both → sync-monitoring → deploy `force_all=true` → gatus green, `curl …/staging/leaderboard/health` → `ok` → add a real rule via admin UI, watch points flow. Vercel: add `VITE_LEADERBOARD_URL`/`VITE_INGESTOR_URL`.

**Workflow**: create SO Jira ticket(s) first; PRs target `staging`, squash-merge, link back to ticket. Suggested split from branch `ewitulsk/leaderboard`: PR1 go-backend services + go-ci, PR2 deployment wiring, PR3 frontend (stacked).

## Risks / open questions
- Candidate-event heuristic (copy+drop, no key) can surface non-event structs — admin confirms; acceptable.
- `asMovePackage` GraphQL shape unverified in-repo — Phase 1 live probe is the gate.
- Package upgrades emit under new ids — v1: admin adds the new package; auto-follow is a follow-up.
- `POST /internal/link` is internal-only; proving twitter↔wallet ownership (OAuth + signature) is future work — DB and merge logic are ready.
- Windowed rank = full ledger aggregation per request — fine at launch volume; rollup tables are the isolated escape hatch in `store.go`.
- Public GraphQL rate limits during backfill — throttled; private fullnode URL is a TOML-only change later.
