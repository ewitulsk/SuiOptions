# Pyth transition — complete usage inventory

**Why this exists:** Pyth is moving off free public access to a paid tier
(~$500/mo), which is out of budget. This document is the exhaustive answer to
"where and how do we use Pyth", so the cost of leaving (or the cost of a
partial carve-out) can be priced.

Scope: `staging` branch, 2026-08-01. Three surfaces — **protocol** (Move,
on-chain), **backend** (Rust services), **frontend** (browser). Every path
below was read, not inferred.

> The covered-call vault (`options_vault`) was the deepest Pyth coupling in
> the codebase — Pyth types in five public function signatures. It was
> deprecated in SO-332: the package is no longer published and nothing
> off-chain drives it, so it is **excluded from this inventory**. See
> `contracts/vault/DEPRECATED.md`. Reviving it would re-introduce that
> coupling and should be priced separately.

---

## 0. TL;DR

| Surface | Pyth dependency | Removable without a redeploy? |
|---|---|---|
| `trading_vault` (curated vault) | Pyth lives in a **separate adapter package** behind a witness allowlist | **Yes.** Publish a new adapter, allowlist its witness, delist `PythOracle` |
| Backend | One process (`oracle-service`) holds the only Hermes SSE + Benchmarks connection; keeper holds one direct Hermes path for the on-chain VAA | Mostly — two seams, both narrow |
| Frontend | **Every browser session opens its own anonymous Hermes SSE stream** and fetches its own accumulator update for deposits | Hardest surface: no key can live client-side |

Three distinct Pyth products are in use, and they fail independently:

1. **Hermes latest/stream** — live spot for quoting, UI, strike selection.
2. **Hermes accumulator (`encoding=base64`)** — the signed Wormhole VAA that
   is *pushed on-chain* to refresh `PriceInfoObject`s. A price cache cannot
   substitute for this; only Pyth can sign it.
3. **Benchmarks (`/v1/updates/price/{ts}`)** — historical daily closes for
   realized-vol (σ), feeding the scheduler's z-ladder strike grid and the
   keeper's on-chain vol-book posts.

The on-chain read itself is nearly free (`update_fee_mist = 1`). The cost is
entirely in getting (2) off Pyth's servers.

**Two facts worth confirming before planning anything:**

- Everything — **staging *and* prod** — runs against Sui **testnet** and
  **`hermes-beta.pyth.network`**, not stable Hermes. Whether the beta network
  is even in scope for the paid gate is unverified.
- Per SO-252 (`rust-backend/crates/runtime-config/src/secrets.rs:33-37`), the
  Pyth API key we already wired gave **zero** rate-limit elevation on the
  public endpoints (keyed 429s identically to unkeyed). The comment in that
  file says a key is "mandatory for Pyth Core access from 2026-07-31" — i.e.
  yesterday. It is not established that we currently hold working Core
  credentials at all.

---

## 1. Protocol layer (Move, on-chain)

There is exactly **one** live Pyth integration in `contracts/`, and it was
built to be swappable.

### 1a. `oracle_pyth` — Pyth is a pluggable adapter (clean exit)

Package: `contracts/oracle-pyth`. This package exists *only* to turn Pyth into
the protocol's neutral `PriceAttestation` type. It is the newer design and it
was built for exactly this scenario.

`contracts/oracle-pyth/sources/oracle_pyth.move`:

- `public struct PythOracle has drop {}` — witness, mintable only here.
- `PythFeedRegistry` — shared object; `Table<TypeName, FeedEntry{feed_id, decimals}>`
  plus `max_age_secs` (default 60) and `max_conf_bps` (default 100). Guardrails
  are registry state, not caller arguments, so a PTB cannot loosen them.
- `attest<Asset, Quote>(feed_reg, oracle_reg, &PriceInfoObject, &PriceInfoObject, clock) -> PriceAttestation`
  — cross math plus feed-ID pinning, staleness and confidence-ratio
  guardrails, timestamped at the **older** of the two legs.
- Admin surface: `set_feed<T>`, `remove_feed<T>`, `set_max_age_secs`,
  `set_max_conf_bps` (all `AdminCap`-gated).

**Why this is the good case:** `contracts/trading-vault/Move.toml` has **no
pyth dependency at all**. `trading_vault::price::attest` requires the witness
to be on the `OracleRegistry` allowlist. So swapping oracle providers is:

1. publish a new adapter package minting its own witness,
2. allowlist it on `OracleRegistry`,
3. delist `PythOracle`.

No `trading_vault` redeploy, no vault migration, no user-visible object churn.

### 1b. Move packages that are *not* Pyth-coupled

Worth stating explicitly, because it bounds the blast radius:

- `contracts/core` (`options_core`) — deliberately dependency-free (Sui
  framework only). The buckets/cursor/accounts/quotes/treasury core is clean.
- `contracts/auction`, `contracts/rfq`, `contracts/trading-vault`,
  `contracts/options-adapter` — no pyth.
- `contracts/equity-oracle` — **keeper-attested**, not Pyth. Guardrailed
  on-chain (poster allowlist, min interval, max delta, ceiling).
- `contracts/options-adapter/sources/vol_book.move` — attested realized-vol
  book. The keeper posts values *derived* from Pyth Benchmarks off-chain, but
  there is no on-chain pyth import. Losing Benchmarks degrades the input, not
  the contract.
- `solana-contracts/programs/*` — no Pyth. (The `pyth-solana-receiver-sdk`
  entries under `services/solana-*/target/` are stale build artifacts from the
  `solana-staging` branch, not a source dependency on this branch.)
- `contracts/vault` (`options_vault`) — **deprecated, not published** (SO-332).
  It still declares `pyth` in its `Move.toml` and `sources/oracle.move` still
  imports it, so a grep will surface it — but the package never reaches chain
  and nothing calls it. Not part of the Pyth exposure.

> **Formerly the sharpest constraint here — now gone.** DeepBook Margin was
> hard-wired to Pyth (`deepbook_margin` entry functions take `PriceInfoObject`s
> by reference, and `mm-bot/desk/dbm.rs` passed them on every margin write),
> and it was Mysten's package on Mysten's deployment — not ours to republish
> against a different oracle. **SO-334 removed the DeepBook Margin integration
> entirely**, taking `contracts/dbm-oracle`, `mm-bot/desk/dbm.rs`, the keeper
> `[external.dbm]` legs and the frontend DBM discovery with it.
>
> Re-verified after that commit: **no third-party package receives a
> `PriceInfoObject` from any PTB we build.** Every remaining Pyth consumer is
> ours, which means Pyth can be retired completely rather than partially.

---

## 2. Backend layer (Rust)

### 2a. `crates/pyth-client` — all Pyth network I/O, in one place

`rust-backend/crates/pyth-client` (~1,670 lines). Nothing else in the backend
speaks HTTP to Pyth.

| module | endpoint / role |
|---|---|
| `http.rs` | `GET /v2/updates/price/latest?ids[]=…` (Hermes); `GET /v1/updates/price/{unix_secs}?ids=…` (Benchmarks). 429 → 60s pause, transient → exponential backoff, 5 attempts. |
| `http.rs::latest_with_update_data` | Same endpoint with `encoding=base64` — returns the **binary accumulator payload**. This is the on-chain push path. |
| `stream.rs` | SSE `/v2/updates/price/stream?ids[]=…`, reconnecting tokio task. |
| `cache.rs` | `PriceCache` / `get_fresh(max_age, max_publish_lag)`. |
| `spot.rs` | Cross math — the off-chain twin of `oracle_pyth.move::cross_from_prices`. |
| `vol.rs` | Realized-vol math. **No network** — pure, keeps working. |
| `sigma.rs` | Realized σ from Benchmarks daily closes. |
| `benchmark_cache.rs` | `BenchmarkVol` — immutable-close cache + bulk multi-`ids` fetch + 1.1s request pacer (SO-253, the actual fix for the 429 storm). |
| `benchmark.rs` | **beta → stable feed-id map.** Sui testnet `PriceInfoObject`s are keyed by beta ids; Benchmarks serves stable only. 5 pairs mapped: BTC, SUI, USDC, DEEP, WAL. |
| `lib.rs::auth_headers()` | `Authorization: Bearer <api_key>`, attached via `reqwest` default headers. `None` → anonymous tier. |

### 2b. `services/oracle-service` — the single Pyth gateway (SO-254)

The architectural win: **one** external Pyth connection for the whole backend.

- Binds `0.0.0.0:9013`.
- `hermes_url = https://hermes-beta.pyth.network`,
  `benchmarks_url = https://benchmarks.pyth.network`
  (identical in `config.staging.toml` and `config.prod.toml`).
- Discovers which feeds to subscribe to from the **token-info catalog** —
  every token carrying a `pythFeedId` (`main.rs:44-60`). Empty set → hard fail
  at boot.
- Opens **one** authenticated SSE subscription, drains into a `PriceCache` +
  an internal WS fanout.
- Serves: `GET /prices`, `/snapshot`, `/prices/:feed`, `/vol/realized`, `WS /ws`
  (`router.rs:24-29`).
- Holds the API key: secret `options/<env>/oracle-service` → `{"pyth_api_key": …}`.
  Key is **optional** — absent key logs a warning and runs anonymous
  (`main.rs:29-40`).

### 2c. Consumers that go through `oracle-client` (no direct Pyth)

`crates/oracle-client` is a hard cutover — there is **no direct-Pyth fallback**
in any consumer. `fetch_blocking_until_ready` retries at boot, then crashes.

| service | transport | what it needs the price for |
|---|---|---|
| `mm-bot` | WS → local `PriceCache` | per-RFQ hot path: `desk/{quote,exits,monitors,auctions,hedge}.rs`. Staleness gates in `[pyth]` config: `max_price_age_ms`, `max_publish_lag_ms`, `max_conf_bps`, `fallback_vol`. Derives its market list from token-info tokens that have a feed (`main.rs:269`). |
| `option-scheduler` | REST | strike selection. `config.*.toml` declares `[[pairs]] source = "pyth"` — 5 pairs on staging, 7 on prod. Both legs must carry a `pythFeedId` or the scheduler **fails at boot** (`spot.rs:76-97`). |
| `market-sim` | WS → `PriceCache` | DeepBook post-only bid/ask band around the Pyth cross (`sim.rs:241`). |
| `keeper` | REST | realized vol for the trading vault's on-chain vol-book posts (`trading_vault.rs:460`). A fetch failure skips the post — no fallback constant on this path. |

`price-charting` still links `oracle-client`, but its only consumer was the
covered-call APY sampler, which is no longer spawned (SO-332). It makes no
Pyth-derived calls at runtime today.

### 2d. `services/keeper` — the one remaining direct Hermes path

The keeper keeps its own `reqwest` client with `pyth_client::auth_headers`
**solely** to fetch the signed accumulator payload. A price cache cannot serve
a VAA.

- `trading_vault.rs:1338` — `latest_with_update_data(hermes_url, feeds)` →
  builds the PTB prefix for attestation-bearing trading-vault appraisals, then
  the crank. This is the only live accumulator fetch in the backend.
- Config `[pyth]` block (`config.staging.toml:20-40`, mirrored in prod):
  `hermes_url`, `pyth_package_id` `0xabf837e9…`, `wormhole_package_id`
  `0xf47329f4…`, `pyth_state_id` `0x243759…`, `price_info_table_id`
  `0xcb858b77…`, `wormhole_state_id` `0x31358d19…`, `update_fee_mist = 1`.
- The keeper's own AWS secret also carries `pyth_api_key`
  (`render-secrets.sh:285`).

### 2e. `crates/sui-tx` — the on-chain push, as a PTB prefix

`crates/sui-tx/src/tx/pyth_update.rs` (227 lines) encodes the Pyth Sui
accumulator flow:

1. off-chain: `extract_vaa_from_accumulator` — pull the Wormhole VAA out of
   the Hermes `PNAU` message (proof type 0 = Wormhole Merkle);
2. `wormhole::vaa::parse_and_verify(wormhole_state, vaa, clock)`;
3. `pyth::pyth::create_authenticated_price_infos_using_accumulator(...)`;
4. `pyth::pyth::update_single_price_feed(...)` — one per feed, fee split from gas;
5. `pyth::hot_potato_vector::destroy<PriceInfo>(potato)`.

Related:

- `crates/sui-tx/src/tx/appraisal.rs` — `pyth_assets_needed()` (line 190)
  computes which held assets need a Pyth leg, then composes the update prefix
  plus one `oracle_pyth::attest` per asset (lines 596-640).
- `crates/sui-tx/src/tx/template.rs` — **gas-station sponsorship allowlist**.
  `PythPkgs` (line 306) registers the four Pyth prefix calls with pinned
  arities (lines 562-591). If the prefix shape changes, sponsored deposits
  stop being sponsored.

### 2f. `services/gas-station`

`[pyth] pyth_package_id / wormhole_package_id` in `config.{toml,staging,prod}.toml`
→ `PythPkgs` (`main.rs:113-119`). Purpose: sponsor the Pyth price-update prefix
on user trading-vault deposits. Without a valid `[pyth]` block, appraised
deposits still work but users pay their own gas for the oracle legs.

### 2g. `services/token-info` — the feed-id registry

Sole source of truth for which feeds anything subscribes to.

- DB column `pyth_feed_id TEXT` (`db/migrations/000001_init/up.sql:14`,
  `db/schema.rs:11`, `db/models.rs:20`).
- `overlay.rs:40` seeds it from the deployment's `token_info` block.
- Served on `/tokens` (`handlers/tokens.rs:125`).

**Feed inventory** (`rust-backend/deployments.json`, identical in `prod` and
`staging` — both are testnet/beta ids):

| ticker | Pyth beta feed id | symbol |
|---|---|---|
| TBTC | `0xf9c0172ba10dfa4d19088d94f5bf61d3b54d5bd7483a322a982e1373ee8ea31b` | Crypto.BTC/USD |
| TSUI | `0x50c67b3fd225db8912a424dd4baed60ffdde625ed2feaaf283724f9608fea266` | Crypto.SUI/USD |
| TUSDC | `0x41f3625971ca2ed2263e78573fe5ce23e13d2558ed3f2e47ab0f84fb9e7ae722` | Crypto.USDC/USD |
| TWAL | `0xa6ba0195b5364be116059e401fb71484ed3400d4d9bfbdf46bd11eab4f9b7cea` | Crypto.WAL/USD |

Plus DEEP (`0xe18bf5fa…`) in the beta→stable map and native SUI (same id as
TSUI) hard-coded in the frontend. **Five distinct feeds total** — that is the
whole surface we would be paying for.

### 2h. Secrets & infra plumbing

- `crates/runtime-config/src/secrets.rs:100-105` — `PythSecrets { api_key: Option<String> }`,
  `[pyth] api_key` section. Optional by design.
- `infra/secrets.tf:178-207` — `options/<env>/oracle-service` secret
  (`{"pyth_api_key": …}`). Terraform writes a `REPLACE_ME` placeholder with
  `ignore_changes`; real value set by hand. The keeper's key-bearing secret was
  created out-of-band.
- `deployment/ec2/render-secrets.sh:48-62` — `append_pyth_api_key()`, appends
  the `[pyth]` section to the rendered TOML. Called for `keeper` (285) and
  `oracle-service` (311).

### 2i. Tools

- `tools/deployment-manager` — on every contract redeploy: publishes
  `oracle_pyth` (`main.rs:271-281`), creates the `PythFeedRegistry`
  (`trading_vault_init.rs:128`), allowlists the `PythOracle` witness (231), and
  calls `set_feed` per catalog token that has a feed (249-260). Records
  `pythFeedRegistryId` + `oraclePyth.packageId` into `deployments.json`.
- `tools/trading-vault-smoke` — end-to-end appraisal smoke test; consumes
  `PythHandles` and `pyth_assets_needed`.
- `tools/exchange` — prints each token's feed id in its catalog dump.

### 2j. Backend services with **no** Pyth dependency

`api-service` (explicitly a pure compute endpoint — the frontend supplies
spot), `indexer`, `auth-service`, `quoting-service`, `cctp-relay`,
`hedge-signer`, `balance-monitor`, `social-bot`, `twitter-service`, and every
`solana-*` service.

---

## 3. Frontend layer (browser)

This is the sharpest problem, because **the browser talks to Pyth directly and
anonymously**. There is nowhere to put an API key client-side without
publishing it.

### 3a. `src/api/pyth.ts` — per-session Hermes SSE

- `export const HERMES_BASE = "https://hermes-beta.pyth.network"`
- `new EventSource(`${HERMES_BASE}/v2/updates/price/stream?ids[]=…`)` — one
  stream per browser session, covering every feed with a live subscriber;
  restarts on subscription-set change.
- `resolveFeedId()` resolves symbols via the token-info catalog
  (`SUPPORTED_TOKENS[].pythFeedId`), with one hard-coded fallback:
  `SUI_FEED_ID = 0x50c67b3f…` (native SUI is an ambient spot symbol, not a
  catalog token).
- Auto-reconnects on error; logs `[pyth] hermes stream error`.

**Billing implication:** this is N connections for N concurrent users, none of
them attributable to our account or coverable by a server-side key.

### 3b. `src/api/usePythPrice.ts` — the React hooks

`usePythPrice(symbolOrFeedId)` and `usePythPrices(symbols[])`. Consumers:

| file | use |
|---|---|
| `src/components/BucketBar.tsx:28` | live spot tick on the bucket bar ("spot live · pyth") |
| `src/state/dashboard.ts:429` | one feed per offered asset, dashboard spot column |
| `src/state/composer.ts:293` | buy-screen spot for the selected asset |
| `src/api/optionMetrics.ts:73` | spot is part of the query key — every Pyth tick refetches metrics |
| `src/api/positionEconomics.ts:93` | notes there is no client-side underlying history (Pyth streams live only) |

### 3c. `src/tx/appraisal.ts` — the browser builds the on-chain Pyth push

The trading-vault deposit flow requires a complete `Appraisal` hot potato in
the same PTB, so the **browser** fetches and pushes the price update:

- `fetch(`${HERMES_BASE}/v2/updates/price/latest?…&encoding=base64`)` (line 606)
- decodes the `PNAU` accumulator, extracts the VAA (line ~630)
- emits `wormhole::vaa::parse_and_verify` (725) →
  `pyth::pyth::create_authenticated_price_infos_using_accumulator` (733) →
  `pyth::pyth::update_single_price_feed` per feed (747) →
  `hot_potato_vector::destroy`
- then one `oracle_pyth::attest<Asset, Dep>` per needed asset.

Hard-coded deployment handles (`PYTH_HANDLES.testnet`, lines 91-98) mirroring
the keeper's `[pyth]` block: pyth pkg `0xabf837e9…`, wormhole pkg
`0xf47329f4…`, pyth state `0x243759…`, wormhole state `0x31358d19…`,
`updateFeeMist: 1n`.

Feasibility pre-flight lives in `src/api/useTradingVaults.ts:160` — the deposit
button is disabled with a reason when an asset has no Pyth feed.

### 3d. `src/config.ts` — the ids

`ORACLE_PYTH_PACKAGE_ID` (88), `PYTH_PRICE_INFO_TABLE_IDS` (125),
`pythFeedRegistryId` (136/234), and per-token `pythFeedId` mapped from
token-info's `pyth_feed_id` (309).

### 3e. Admin UI

`src/api/tokenAdmin.ts` + `src/components/TokenManager.tsx:234-261` — the
"Pyth feed id (optional)" field when registering a token.

---

## 4. What breaks, in order of severity, if Pyth access is cut

1. **Trading-vault deposits stop entirely.** `vault::deposit` only accepts a
   complete `Appraisal`; every non-deposit asset needs an attestation; the only
   allowlisted attestation source is `PythOracle`, which needs a fresh
   `PriceInfoObject`, which needs a Hermes accumulator. No Hermes → no deposits.
   (Affects browser *and* keeper.)
2. **mm-bot stops quoting.** `get_fresh` declines on an aged cache; RFQs are
   rejected rather than mispriced. Fails safe, but the desk goes dark.
3. **option-scheduler stops rolling.** Pyth-sourced pairs cannot resolve spot;
   rolls skip. (Note: it *fails at boot* if a leg lacks a `pythFeedId`.)
4. **market-sim stops banding**, so testnet DeepBook books go empty.
5. **Frontend spot goes dead** on the dashboard, buy screen, and bucket bar;
   option-metrics queries stall on a null spot.
6. **Degraded, not dead:** realized-vol (Benchmarks) loss is already
   load-bearing on testnet today, because Benchmarks serves stable ids and we
   run beta ids. The scheduler's z-ladder falls back to the per-pair
   `sigma_fallback` (0.85 on prod); the keeper simply skips its vol-book post,
   leaving the last on-chain σ in place.

---

## 5. Exit paths — sketch only, not researched

Listed to frame the decision, not as a recommendation. Each needs its own
spike.

**Cheapest first:**

- **Confirm the beta network is actually gated.** Everything we run points at
  `hermes-beta.pyth.network`. If beta stays free, the immediate exposure may be
  zero and this becomes a mainnet-launch problem rather than a now problem.
- **Run our own Hermes.** Pyth's Hermes is open-source and pulls from Pythnet;
  self-hosting removes the hosted-endpoint dependency while keeping every
  on-chain contract, feed id, and VAA path byte-identical. This is the only
  option that requires **zero** contract changes. Cost moves from subscription
  to ops.
- **Proxy the browser through `oracle-service`.** Independent of provider
  choice, and worth doing regardless: it collapses N anonymous per-user streams
  into the one keyed backend connection. Requires a new SSE/WS endpoint on
  oracle-service plus swapping `HERMES_BASE` in `src/api/pyth.ts`. Does *not*
  solve `src/tx/appraisal.ts`, which needs the signed accumulator.

**Expensive:**

- **Swap the oracle provider for the curated trading vault.** Genuinely clean —
  new adapter package, allowlist witness, delist `PythOracle` (§1a). Whatever
  replaces it must produce an equivalently signed, on-chain-verifiable price
  object. With the covered-call vault deprecated, this is now the *only*
  contract-side swap on the table.

---

## Appendix: full file index

**Protocol**
```
contracts/oracle-pyth/Move.toml                pyth git dep + testnet dep-replacement
contracts/oracle-pyth/sources/oracle_pyth.move ENTIRE MODULE — PythOracle witness, PythFeedRegistry, attest
contracts/oracle-pyth/tests/oracle_pyth_tests.move

(contracts/vault/** — deprecated + unpublished, SO-332; declares pyth but is
 not part of the exposure. See contracts/vault/DEPRECATED.md.)
```

**Backend**
```
rust-backend/crates/pyth-client/                 ALL Pyth network I/O (10 modules)
rust-backend/crates/oracle-client/src/lib.rs     internal gateway client (REST + WS)
rust-backend/crates/protocol-types/src/pyth_id.rs  PriceFeedId type
rust-backend/crates/sui-tx/src/tx/pyth_update.rs   Hermes → PTB prefix builder
rust-backend/crates/sui-tx/src/tx/appraisal.rs     pyth_assets_needed + attest composition
rust-backend/crates/sui-tx/src/tx/template.rs      gas-station sponsorship allowlist for pyth legs
rust-backend/crates/runtime-config/src/secrets.rs  [pyth] api_key
rust-backend/services/oracle-service/**            THE gateway: SSE + Benchmarks + fanout
rust-backend/services/keeper/src/trading_vault.rs  :1338 direct Hermes accumulator fetch; :460 vol-book σ
rust-backend/services/keeper/src/config.rs         [pyth] block
rust-backend/services/keeper/config/config.*.toml  hermes_url + on-chain pyth/wormhole ids
rust-backend/services/mm-bot/src/{main,pricing,sim}.rs, src/desk/*.rs
rust-backend/services/option-scheduler/src/{config,spot}.rs + config/config.*.toml
rust-backend/services/market-sim/src/{config,sim,main}.rs
rust-backend/services/gas-station/src/{config,main}.rs + config/config.*.toml
rust-backend/services/token-info/src/{overlay,db/*,handlers/tokens}.rs
rust-backend/tools/deployment-manager/src/{main,trading_vault_init,json_store}.rs
rust-backend/tools/trading-vault-smoke/src/main.rs
rust-backend/tools/exchange/src/main.rs
rust-backend/deployments.json                      pythFeedId per token, pythFeedRegistryId, oraclePyth
rust-backend/infra/secrets.tf                      options/<env>/oracle-service
rust-backend/deployment/ec2/render-secrets.sh      append_pyth_api_key()
```

**Frontend**
```
frontend/src/api/pyth.ts               browser Hermes SSE client, HERMES_BASE
frontend/src/api/usePythPrice.ts       usePythPrice / usePythPrices
frontend/src/tx/appraisal.ts           Hermes REST fetch + wormhole/pyth PTB prefix + attest
frontend/src/config.ts                 ORACLE_PYTH_PACKAGE_ID, PYTH_PRICE_INFO_TABLE_IDS, pythFeedId
frontend/src/state/dashboard.ts        :429 spot per asset
frontend/src/state/composer.ts         :293 buy-screen spot
frontend/src/state/tradingVault.ts     :54 async builder note (Hermes fetch during deposit)
frontend/src/components/BucketBar.tsx  :28 live spot tick
frontend/src/api/optionMetrics.ts      spot in query key
frontend/src/api/positionEconomics.ts  spot history note
frontend/src/api/useTradingVaults.ts   :160 appraisal-plan feasibility
frontend/src/api/tokenAdmin.ts         pyth_feed_id field
frontend/src/components/TokenManager.tsx  :234-261 admin form
```
