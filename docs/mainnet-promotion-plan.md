# Promoting `main` to a mainnet production stack

**Audited at `3929ff3`, 2026-07-31**, against live AWS (account `502186568577`,
us-east-1) and the current `deployments.json`.

**Scope decisions (2026-08-01):**

- Not deployed to prod: `cctp-relay`, `social-bot`, `twitter-service`,
  `market-sim` — already structurally excluded (§7).
- **No `price-charting`** on mainnet (§2.2).
- **No secondary-market listing.** Option coins do not get DeepBook pools (§2.2).
- **No mm-bot.** No protocol-run maker on the core options product; liquidity
  comes later, gradually (§2.3).
- **The Move covered-call vault is deprecated**, superseded by the curated
  trading-vault (vault-curator) product. It is not offered on mainnet (§2.1).

Together these take prod from 14 declared services to **11**, and remove the
only blocker that gated launch. What remains is mostly mechanical.

---

## 0. The one-paragraph version

`prod` today is **not a staging-shaped environment awaiting a network flip.** It
is a *second testnet deployment*, six weeks stale (115 commits behind `main`),
holding a testnet package, testnet faucet tokens, beta Pyth feeds, and a
house-published DeepBook. Promoting it to mainnet is three separable jobs that
are easy to conflate:

1. **Catch prod up to `main`** — a normal deploy, on testnet, changing nothing
   about the network. Low risk, do it first, and it de-risks everything after.
2. **Flip the network** — ~14 config surfaces, a fresh contract publish, a real
   token catalog, real Pyth ids. Mechanical, but the *ordering* has a trap (§3),
   and the workflow that would do the publish refuses to run on mainnet by
   design (§2.4, §6).
3. **Make it safe to hold customer money** — was the long pole. **The scope
   decisions have retired it.** Deprecating the Move vault removes the only
   capital-stranding blocker outright, and the product that does hold customer
   money on mainnet — the curated trading-vault — has the recovery lever the
   Move vault lacked (§2.1).

Nothing now gates launch on a Move change. What remains is configuration,
infrastructure, and a contract publish.

The four scope decisions compound better than they look: dropping DeepBook
removes the largest cost item, dropping `price-charting` follows from it for
free, dropping mm-bot removes a service that would have been a no-op anyway, and
deprecating the Move vault removes the blocker *and* the proceeds-swap
consequence that dropping mm-bot would otherwise have created (§2.3). Prod goes
from 14 declared services to 11.

---

## 1. What `prod` actually is right now

| | staging | prod |
|---|---|---|
| EC2 host | `i-0a8a3c28b87effa81` `options-host` t3a.medium | `i-06240b5c371e8a9dc` `options-prod-host` t3a.medium |
| Sui network | testnet | **testnet** |
| Package | `0xfe9cddb5…` (2026-07-26) | `0x9912263d…` (**2026-06-19**) |
| Deploy marker | `ec2b714` — 13 commits behind `main` | `cbf2a02` — **115 commits behind `main`** |
| Services declared | 15 | 14 today → **11** on mainnet (§7) |
| RDS | `options-db` | **the same `options-db`** |
| ALB | `options-alb` | **the same `options-alb`** |
| Gatus monitors | 8 | **0** |

Shared: one VPC, one ALB (`options-ingress-prod` / `options-ingress-staging`
target groups), one RDS instance (`db.t4g.micro`, 20 GB, **MultiAZ off,
deletion protection off**, 7-day backups, Performance Insights off), separated
only by database name (`indexer_prod` vs `indexer_staging`).

Prod's package predates the four-package split's later work and the whole SO-299
→ SO-330 arc. Whatever else happens, **prod has never run most of what is on
`main`.**

---

## 2. The former blockers — all four now discharged by scope

Each of these gated launch in the first draft of this audit. None does now, and
none was fixed by writing code. Kept in full rather than deleted, because each
records *why* the decision is safe and what would make it unsafe again — the
scheduler default in §2.3 in particular is a trap someone will otherwise walk
back into.

### 2.1 ~~The user vault strands capital~~ — discharged by deprecating the product

**Decision: the Move covered-call vault (`contracts/vault`) is deprecated,
superseded by the curated trading-vault product. No Move vault is created on
mainnet.**

This retires the only launch blocker, and it retires it in the strongest
available way. [`fund-egress-audit.md`](./fund-egress-audit.md) found that
`vault::complete_withdraw` → `finalize_round` needs a live Pyth cross, while the
AdminCap escape hatch `update_oracle_feeds` asserts `phase == Settling` — a phase
that, in a round where nothing sold, is unreachable without the oracle it is
blocked on. `max_price_age_secs` is set at `create_vault` and has no setter, so
the bound could not be widened either. The fixes were real but each needed a Move
change plus a package upgrade — and every upgrade cap is `version = 1`, meaning
that operation has never once been performed.

**A vault that is never created cannot strand anything.** The wedges remain true
of the package; they become unreachable rather than fixed. That is a better
outcome than patching a product being retired anyway.

*Deprecate the product, not the package.* Still **publish** `options_vault` on
mainnet — `indexer/src/main.rs:70` hard-fails at boot without the vault package
id, and paying one publish tx is cheaper than changing the indexer's boot
contract for a package we still publish on staging. A published package with zero
vault objects holds zero funds.

*How it's switched off.* `option-scheduler/src/config.rs:123` —
`vault_template: Option<VaultTemplate>`, and `:256` says *"an empty
`[vault_template]` table is enough to switch vault creation on."* **Omit the
`[vault_template]` table from `config.prod.toml`** and the hourly vault-ensure
pass never runs. No vault is ever created; the keeper's Move-vault crank finds
none in the indexer's `vaults` view and idles.

*The curator product is unaffected, and does not inherit the flaw.*
`contracts/trading-vault/Move.toml` depends on `options_core` alone — **not** on
`options_vault`. The two are cleanly severable. And the audit rates trading-vault
*"conditional, recoverable"*: its staleness bound moves on the spot via
`registry::set_max_price_age_ms` (`registry.move:124`, immediate, like all
eleven of that registry's setters). The lever that was missing on the Move vault
is present here. That is now the product that holds customer money, and it is the
right one to be holding it.

*What else comes off.* Frontend `Vault.tsx`, `tx/vault.ts`, and the Move-vault
half of `api/useVaults.ts` should be hidden on mainnet — they resolve against
`VAULT_PACKAGE_ID` and would render a live-looking but unusable product. The
trading-vault screens (`TradingVaults.tsx`, `TradingVaultDetail.tsx`,
`curator/SetupWizard.tsx`) use `TRADING_VAULT_PACKAGE_ID` exclusively and are
untouched — verified, the two never cross.

*One carried-forward item.* The upgrade path is still unrehearsed
(`version = 1, policy = 0` on every cap; no Move upgrade wrapper in any of the 85
`.move` files; signing services reject `Command::Upgrade` in four places). It no
longer gates launch, but it is the prerequisite for
`upgrade-contract-mainnet.yml` — see §6.

**Keep the keeper.** It is not just the Move-vault crank: `keeper/src/main.rs:120`
builds a `trading_vault` context and `:216` runs a trading-vault tick each pass
(equity posting into `EquityBook` / `dbm_oracle`, SO-299). That context is a
**fatal** dependency — the keeper crashes if the trading-vault family is absent.
It stays deployed and stays load-bearing for the curator product.

### 2.2 ~~DeepBook cost~~ — resolved by dropping the secondary listing

**Decision: no DeepBook pools on mainnet.** Recording what this avoids, how it's
switched off, and the one service that doesn't survive it.

*What it avoids.* Each option strike gets its own pool, and permissionless pool
creation costs **500 real DEEP**. As `config.prod.toml` is written today — hourly
TSUI on a 6-point z-ladder plus five weekly families — that is ~148 pools/day,
≈ **74,000 DEEP/day, burned, forever.** This was the largest line item in the
mainnet operation and it was a config default nobody had chosen. It is now gone.

*How to switch it off.* Cleanly, and with no code change:
`option-scheduler/src/main.rs:124` reads `snapshot.deepbook()`, which is
`Option`. **Omit the `deepbook` block from `deployments.json::prod`** and the
scheduler logs `"token-info reports no DeepBook deployment — rolling buckets
without pools"` and rolls buckets without them. This is the single switch — it
turns pool creation off everywhere at once, rather than per-service.

*Who degrades gracefully.* Verified `Option`-handling, no action needed:

| service | behaviour without DeepBook |
|---|---|
| `option-scheduler` | rolls buckets, creates no pools (`main.rs:124-145`) |
| `indexer` | `PoolCreated` ingestion simply off (`main.rs:77`) |
| `gas-station` | no DeepBook PTB templates sponsored (`main.rs:74`) |
| `mm-bot` | n/a — not deployed (§2.3) |

*Who does not — and is also being dropped.* **`price-charting` treats a missing
DeepBook as fatal**: `main.rs:41` does
`.context("token-info reports no DeepBook deployment for this network")?`,
because a chart service with nothing to chart has no job. **Decision: it is not
deployed to mainnet either.** Undeclare it from `docker-compose.prod.yml` — it is
health-gated there today, so leaving it declared would roll back every deploy.

Dropping it is the cheaper half of the same decision, and it removes three other
problems at once: the paused-Tiger-instance landmine (§5), the need for any prod
Timescale instance, and the `options/prod/price-charting` secret.

Downstream, cleanly:

| consumer | behaviour |
|---|---|
| `api-service` | `derived_metrics_url: Option<String>` (`config.rs:27`) — omit it and `/vaults/:id/apy` serves realized-only |
| nginx | drop the `location ~ ^/prod/charts` block (`nginx.prod.conf:111`) |
| frontend | `CHARTS_URL` is read only by `config.ts` and `api/charts.ts`; chart surfaces render empty |
| Vercel | remove `VITE_CHARTS_URL` from Production |

Knock-ons to accept: no option-price charts on mainnet, and no remaining
justification for the hourly `[pairs.grid]` cadence — that family existed largely
to give the book depth. Revisit it in §4.

### 2.3 ~~mm-bot inventory~~ — resolved by not running it

**Decision: no mm-bot on mainnet.** Removing it is genuinely simpler — it is not
declared in prod's compose, so this is a no-op for deployment, and
`options/prod/mm-bot` should be deleted so `render-secrets.sh` skips it and
`balance-monitor` stops watching a wallet that doesn't exist.

It also removes a problem rather than creating one: `FaucetLiquiditySource` is
the only `LiquiditySource` wired in (`main.rs:485`), it builds from `testTokens`,
and token-info serves none on mainnet — so the bot would have been a no-op for
every coin anyway (`liquidity.rs:78`). Not running it is more honest than running
it empty.

One consequence, now moot but worth recording because it would have been a
blocker: **mm-bot is the only bidder on the Move vault's proceeds swap.**
`vault.move:668-675` — with `hold_premium_in_settlement == false` (the
scheduler's default, `config.rs:303`), `finalize_round` asserts `residual_s == 0`,
so every unit of premium must be swapped to underlying via an auction that needs
a maker. No maker → settlement returns to proceeds (`:608-616`) → the keeper
re-opens → **finalize aborts forever** → `complete_withdraw` never pays.

That would have stranded capital in every mainnet vault. It does not apply here
only because §2.1 creates no Move vaults at all. **If the Move vault is ever
revived without a maker, this is a launch blocker** — the fix is
`hold_premium_in_settlement = true` at `create_vault`, and it must be set at
creation, because `update_config` lands at `finalize_round`, the blocked
operation. Worth fixing the Rust default regardless, so staging stops creating
vaults with a latent wedge.

Accept, on the core options product: with no maker, RFQs go unfilled. That is the
intended gradual-rollout posture — the product is live and quotable, with no
protocol-side counterparty until liquidity is introduced deliberately.

The SO-299 vol desk is separately off (`[desk] enabled = false`) and stays off.

### 2.4 The publish path refuses to run on mainnet — by design

This is the ordering trap, and it is worth internalizing before touching a
config.

SO-330 made the mainnet guards real. `assert_testnet.sh` derives the network
from `services/*/config/config.<env>.toml` and refuses anything it cannot prove
is testnet. It gates **both** `redeploy-contract.yml` and `wipe-provision-db.yml`,
as their first step.

So the moment you flip the prod configs to `mainnet`:

- `redeploy-contract.yml` — the *only* automated path that publishes the package,
  resolves the publish checkpoint, rewrites `start_checkpoint`, and wipes+reprovisions
  `indexer_prod`/`scheduler_prod` — **refuses on prod.**
- `wipe-provision-db.yml` — **refuses on prod.**

That is correct behaviour and should not be weakened. But it means:

> **The testnet data in `indexer_prod` and `scheduler_prod` must be wiped BEFORE
> the configs flip, and the mainnet publish needs a path that isn't
> `redeploy-contract.yml`.**

Get this backwards and you land in a state where prod carries testnet indexer
rows and stale scheduler rolls, and the tool that clears them refuses to run.
Recovery is manual `psql` against RDS.

Do **not** add a mainnet bypass to the guarded workflows. Split them instead —
§6.

---

## 3. The network flip — every surface

Fourteen places. `resolve_network.py` requires **unanimity** across service
configs, so a partial flip fails closed and blocks the guarded workflows. Land
these as one commit.

**Service configs** (`services/*/config/config.prod.toml`) — `network = "testnet"` → `"mainnet"`:
`token-info`, `indexer`, `gas-station`, `hedge-signer`, `balance-monitor`, plus
`mm-bot`, `price-charting` and `market-sim` — **all three are undeployed, but
`resolve_network.py` reads every `config.prod.toml` on disk regardless.** Flip
them or delete the files; leaving them at `testnet` breaks unanimity and jams
every guarded workflow. Flipping is the smaller change and keeps the configs
usable if a service is ever revived.

**Endpoints and third-party ids:**

| file | field | testnet → mainnet |
|---|---|---|
| `indexer/config.prod.toml` | `remote_store_url` | `checkpoints.testnet.sui.io` → `checkpoints.mainnet.sui.io` |
| `indexer/config.prod.toml` | `start_checkpoint` | `350202416` → mainnet publish checkpoint − 1 |
| `api-service/config.prod.toml` | `sui_graphql_url` | `graphql.testnet` → `graphql.mainnet` |
| `oracle-service/config.prod.toml` | `hermes_url` | `hermes-beta` → `hermes.pyth.network` |
| `keeper/config.prod.toml` | `[pyth] hermes_url` | `hermes-beta` → `hermes.pyth.network` |
| `keeper/config.prod.toml` | `[pyth]` pkg/state/wormhole/price-info-table ids | all four → mainnet ids |
| `gas-station/config.prod.toml` | `[pyth]` pkg + wormhole | → mainnet ids |
| `hedge-signer/config.prod.toml` | `[bluefin_proxy]` × 3 | `api.sui-staging.bluefin.io` → Bluefin mainnet |

**Feed ids are not a rename.** Sui testnet `PriceInfoObject`s are keyed by the
**beta** feed set; mainnet uses the **stable** set. Every `pythFeedId` in
`deployments.json::prod.token_info` changes, and `oracle-service`'s beta→stable
request mapping becomes a no-op — verify it doesn't double-map.

**Code, not config:**

- `deployment/ec2/render-secrets.sh:34` — `case "$ENV" in prod) NETWORK=testnet`.
  This picks the `[sui]` key slot every service key is rendered into. Flip it and
  the two Dockerfiles below **together**, or services boot with a key in a slot
  they don't read.
- `Dockerfile.keeper:39` and `Dockerfile.scheduler:45` — `case "$APP_ENV" in
  staging|prod) NET=testnet`. Both need prod → mainnet.

**Frontend** (`frontend/src/config.ts`) — three constants have a `testnet:` key
and no `mainnet:` one; on mainnet the DBM equity leg and Pyth feed resolution
silently return `undefined`:

- `DBM_MARGIN_REGISTRY_IDS`
- `DBM_ORIGINAL_PACKAGE_IDS`
- `PYTH_PRICE_INFO_TABLE_IDS`

Vercel (`pismo-protocol/sui-options`, Production): flip `VITE_ENVIRONMENT` to
`mainnet`, and **add `VITE_HEDGE_SIGNER_URL` — it is absent from Production
today**, so the curator dashboard's FROST and Bluefin relay paths fall back to
`127.0.0.1:9017` and fail in the browser.

Everything else on the frontend is already ENV-driven and degrades correctly:
the faucet page renders a "testnet only" notice, the header testnet chip
disappears, `TEST_TOKENS` empties, and Sui RPC/GraphQL URLs are keyed by network
in `suiGrpc.ts`.

---

## 4. The token catalog is a from-scratch build

Not a flip — a different thing entirely.

On mainnet `token-info` sets `overlay_test_tokens() == false` (`config.rs:90`)
and serves **the durable DB catalog alone**. `deployments.json::prod.token_info`
today lists TBTC/TSUI/TUSDC/TWAL pointing at faucet packages. All of it goes.

You need, for each real asset: mainnet coin type, correct decimals, and the
**stable** Pyth feed id — then seeded into `token_info_prod` through
auth-service's admin JWT and token-info's mutate port. There is no auto-seed on
mainnet, deliberately.

`option-scheduler`'s `[[pairs]]` and `mm-bot`'s `settlement_symbol = "TUSDC"`
both reference tickers by name and must be rewritten to the real ones. Note the
frontend's `findToken()` strips a leading `T` as a display alias — harmless, but
it means a real `TUSDC` collision would resolve oddly.

Decide which assets actually launch. The current five families (BTC/WAL/SUI
calls, BTC/SUI puts, weekly + hourly) is a lot of surface for a first mainnet
day and directly drives §2.2.

---

## 5. Infrastructure gaps

**RDS is the sharpest one.** Real-money prod would share a `db.t4g.micro` with
staging, with no MultiAZ, no deletion protection, no Performance Insights, and
7-day backups. Staging load, a staging migration, or a staging wipe fired at the
wrong target all reach prod's data. Before mainnet: a **separate RDS instance**
for prod, MultiAZ on, deletion protection on, retention raised, and a right-sized
class. This is the one item I would not compromise on.

**Gatus has zero prod monitors** — all 8 are staging. Prod is unmonitored by the
status page today.

**`price-charting` was a deploy landmine twice over** — declared in prod's compose
and therefore health-gated, while (a) `options/prod/price-charting` points at
*staging's* paused Tiger Data instance and (b) it treats a missing DeepBook
deployment as fatal. Both are resolved by dropping the service (§2.2). Undeclare
it from `docker-compose.prod.yml` **before the next prod deploy** — this is the
one item here that bites during Phase 1, on testnet, not only at mainnet.

**Prod host disk:** 100 GB gp3, same as staging. Staging filled its original 50 GB
with unpruned containerd snapshots and wedged SSM. There is still no prune and no
alarm. Add a disk alarm before prod carries money.

**Access hygiene:** `allowed_origins = ["*"]` on every public prod service, and
`auth-service/config.prod.toml` `admin_addresses` is `0xab8d1b5a…` — the
**staging deployer address**, in prod's admin allowlist. Both configs carry a
"replace before going live" comment. This is going live.

**Secrets:** every `options/prod/*` key is a testnet-era key. Mainnet wants fresh
keys, generated for the purpose, funded, and with the old ones never reused:
`gas-station` (sponsor — real SUI, sized to expected volume),
`scheduler` (deployer/AdminCap — real SUI; **no DEEP**, per §2.2),
`keeper` (gas), `hedge-signer` (multisig member),
`indexer` (db), `auth-service` (JWT), `sui-rpc` (a **paid** mainnet endpoint —
public fullnodes prune history and Sui deprecated JSON-RPC on them 2026-07-30;
this took the bridge down once already), `oracle-service` (Pyth key — note SO-252
found the key gives zero rate-limit elevation on public Pyth endpoints, so size
expectations accordingly). No `price-charting` and no `mm-bot` secret — both
services are out (§2.2, §2.3); **delete `options/prod/mm-bot`** rather than
leaving a funded testnet key in Secrets Manager.

Also: `hedge-signer`'s FROST shares live on a docker volume, not in Secrets
Manager, and are re-generated per vault by ceremony. Plan the mainnet ceremony
and back up `/app/data/frost-shares.toml` — losing that volume loses the
vaults' co-signing half.

---

## 6. Deploy workflows — rename and rewrite

### Why the existing one cannot be adapted

`redeploy-contract.yml` (625 lines) is a *testnet* tool in its bones, and three
of its steps are actively dangerous on mainnet:

| step | on testnet | on mainnet |
|---|---|---|
| `:429` Reset databases on EC2 | wipes `indexer_*` + `scheduler_*` | destroys real indexed history |
| `:278` `deploy_tokens` | publishes TUSDC/TBTC/TWAL/TSUI faucets | meaningless; must not exist |
| `:261` Deploy contract | publishes a **new** package | **orphans every vault holding funds** |

That last row is the one that matters. On testnet, republish-and-wipe is the
right primitive because the money is fake. On mainnet, republishing a package
whose vaults hold deposits is indistinguishable from destroying those deposits —
`fund-egress-audit.md` puts it plainly: *"republish does not migrate — new ids,
old vault orphaned with the money in it."*

So the mainnet need is not a guarded variant of this workflow. **After genesis,
mainnet never republishes — it upgrades.** That is a different operation, and we
have never performed it (`version = 1` on every cap).

### Proposed shape — three workflows

```
redeploy-contract-testnet.yml     renamed from redeploy-contract.yml, behaviour unchanged
publish-contract-mainnet.yml      NEW — genesis publish only, run at most once per package
upgrade-contract-mainnet.yml      NEW — the ongoing path; does not exist today
```

**1. `redeploy-contract-testnet.yml`** — rename only, no logic change.

- Keep `assert_testnet.sh` as step 0. Once prod's configs say mainnet, the
  `prod` choice in its `environment` input starts refusing, and the workflow
  naturally narrows to `staging`. Leave the choice in place — the refusal is
  more informative than a missing option.
- Rename `wipe-provision-db.yml` → `wipe-provision-db-testnet.yml` for the same
  reason. Both names then say what the guard already enforces.
- Update the `uses:`/docs references and the SO-330 comment block, which
  currently reads as though prod may become mainnet under this same file.

**2. `publish-contract-mainnet.yml`** — genesis only. Differences from the
testnet one, each deliberate:

| | why |
|---|---|
| `assert_mainnet.sh` — the inverse guard | refuse unless *provably* mainnet, so it can never fire at staging. Same `resolve_network.py`, opposite comparison; both fail closed on ambiguity |
| **no DB wipe step at all** | not "guarded" — absent. A step that must never run should not exist to be misconfigured |
| **no `deploy_tokens` input** | no faucets on mainnet |
| **refuses if `deployments.json::prod.package_info` already exists** | genesis means genesis. Overriding needs an explicit `i_am_republishing_and_accept_orphaned_funds` input, spelled out in full |
| GitHub Environment with **required reviewers** | a second human on the one irreversible action |
| typed confirmation input (`CONFIRM=publish-mainnet`) | defeats muscle-memory re-runs |
| **publishes only — does not roll services** | separates "new package exists" from "prod now uses it". Commit `deployments.json`, review the diff, then a normal `Deploy prod` |
| writes `start_checkpoint` from the publish checkpoint | same as testnet; safe here only because the DB is fresh at genesis |
| `deepbook` block **omitted** from the emitted `deployments.json` | §2.2 — this is where the no-secondary-listing decision is enforced |

**3. `upgrade-contract-mainnet.yml`** — the one that actually gets used, and the
one that does not exist.

This is blocked on prerequisites that are not CI work, and the workflow should
not be written before they land:

- No Move-side upgrade wrapper exists in any of the 85 `.move` files.
- Our signing services reject `Command::Upgrade` in four places, by policy.
- The procedure has never been rehearsed at any scale.

Sequence: rehearse `sui client upgrade` by hand on testnet against a **funded
trading vault** (Phase 1, step 6), write the runbook from that, and only then
automate the runbook. Automating an unrehearsed procedure
produces a workflow nobody trusts enough to run in the incident it was built
for.

Until it exists, mainnet contract changes are a documented manual procedure with
a named owner. That is an acceptable interim state; an unrehearsed automated one
is not.

### Shared pieces

- `assert_mainnet.sh` sits beside `assert_testnet.sh` and shares
  `resolve_network.py`. Do not fork the resolver — the whole point of SO-330 is
  that the guard moves with the fact.
- Add a `test_resolve_network.py` case asserting the two guards are exhaustive
  and mutually exclusive over `{mainnet, testnet, devnet, localnet, ambiguous}`:
  every input is accepted by at most one and refused by at least one.
- `_deploy.yml` is network-agnostic and needs no changes. `deploy-prod.yml`
  stays as-is — rolling images is safe on mainnet; it is only *publishing* that
  is not.

## 7. What prod runs — and what it doesn't

The mechanism throughout is the same: `deploy.sh` filters the requested set
against `docker compose config --services`, so **omitting a service from
`docker-compose.prod.yml` is the opt-out.** An undeclared service can never be
planned, rolled, or health-gated.

**Deployed (11):** `token-info`, `auth-service`, `indexer`, `oracle-service`,
`quoting`, `option-scheduler`, `api-service`, `gas-station`, `hedge-signer`,
`balance-monitor`, `keeper`, behind `nginx`.

**Not deployed:**

| service | status |
|---|---|
| `cctp-relay` | Already undeclared (SO-329); no prod secret. |
| `twitter-service` | Already undeclared; no prod secret. |
| `social-bot` | Already undeclared; no prod secret. |
| `market-sim` | Already undeclared; no prod secret. Synthetic retail flow has no place on mainnet. |
| `mm-bot` | Already undeclared. **Delete `options/prod/mm-bot`** (§2.3). |
| `price-charting` | **Declared today — must be removed** (§2.2). Delete `options/prod/price-charting`, drop the nginx `/prod/charts` route and api-service's `derived_metrics_url`. |

Two traps in that table:

1. **`price-charting` is the only one requiring action**, and it fails *closed*
   in the worst way — it is health-gated today, so the next prod deploy rolls
   back until it is undeclared. Do this first (Phase 1, step 1).
2. **`resolve_network.py` reads every `config.prod.toml` on disk**, deployed or
   not. `mm-bot`, `price-charting` and `market-sim` all still have one, and all
   three must be flipped to `mainnet` (or deleted) or unanimity breaks and every
   guarded workflow jams (§3).

`contracts/vault` is a third category: still **published** on mainnet (the
indexer requires its package id at boot) but never **instantiated** — no vault
object, no funds, no exposure. See §2.1.

---

## 8. Rollout

### Phase 0 — decide (blocks everything)

1. Which assets launch, at which cadences → §4's catalog. With no secondary
   listing, the hourly TSUI family has lost most of its rationale; weekly-only
   is the smaller, more defensible launch.
2. Separate prod RDS: yes (recommended) or accept the shared instance in writing.
3. Confirm the workflow split in §6, and who owns the manual upgrade procedure
   until `upgrade-contract-mainnet.yml` exists.
4. Decide how the Move vault is presented on mainnet — hidden entirely, or
   shown with a deprecation notice. `Vault.tsx` currently renders against
   `VAULT_PACKAGE_ID` and would look live (§2.1).

### Phase 1 — catch prod up, still on testnet

Nothing here touches the network, and all of it is reversible.

1. **Undeclare `price-charting` from `docker-compose.prod.yml` first** (§2.2) —
   its secret points at a paused DB, so leaving it declared fails the health gate
   and rolls back the whole set before anything else gets a chance to run.
2. `Deploy prod` with `force_all` at `main`. This is a 115-commit jump — expect
   surprises, and find them on testnet.
3. `redeploy-contract-testnet` on prod (`rebuild-all`) to bring the prod testnet
   package up to `main`'s contracts. Still permitted — prod is provably testnet.
4. Verify end to end on prod-testnet: a roll, a **trading-vault** deposit →
   appraisal → withdraw cycle, a keeper crank (both passes), gas-station
   sponsorship, hedge-signer FROST.
5. Confirm the Move vault is genuinely dormant: with `[vault_template]` omitted,
   no `VaultCreated` event fires and the keeper's Move-vault pass idles while its
   trading-vault pass keeps running.
6. **Rehearse the package upgrade on a funded trading vault.** No longer a launch
   gate (§2.1), but it is the prerequisite for `upgrade-contract-mainnet.yml`,
   and this is the only environment where a failed rehearsal costs nothing.

Exit: prod-testnet runs `main`, the curator flow has been exercised end to end,
and the upgrade procedure has been performed at least once.

### Phase 2 — build mainnet infrastructure alongside

Runs in parallel with Phase 1; touches no existing environment.

1. New RDS instance, MultiAZ, deletion protection, sized. Provision
   `indexer_prod`, `scheduler_prod`, `token_info_prod` by hand — the wipe
   workflow excludes prod and will refuse post-flip anyway.
2. ~~New prod Timescale instance~~ — not needed; `price-charting` is out (§2.2).
3. Generate every mainnet key; write to `options/prod/*`; fund the gas wallets.
   **No DEEP needed** (§2.2). **Delete `options/prod/mm-bot` and
   `options/prod/price-charting`** so `render-secrets.sh` skips them and
   `balance-monitor` stops watching a wallet that no longer exists.
4. Paid mainnet Sui RPC endpoint → `options/prod/sui-rpc`.
5. Add prod monitors to `gatus-config.yml`; add a disk alarm; sync monitoring to
   both hosts.
6. Replace `admin_addresses` with the real mainnet admin wallet(s); scope
   `allowed_origins` to the real frontend origin.
7. Write `assert_mainnet.sh` and the two new workflows (§6), with the
   `test_resolve_network.py` exhaustiveness case.

### Phase 3 — the flip (ordering is load-bearing)

Do these in this order. §2.4 explains why.

1. **Wipe `indexer_prod` / `scheduler_prod` while still testnet-declared** — via
   `wipe-provision-db-testnet.yml`, which still passes its guard. (Skip if
   Phase 2 provisioned a fresh RDS instance; then it's just a cutover of
   `DB_HOST`.)
2. Stop the prod stack. Wallet-sharing services must be down before a publish.
3. **Publish to mainnet** via `publish-contract-mainnet.yml` (§6). Emit
   `deployments.json::prod` with **no `deepbook` block** and **no `testTokens`**.
   Record every id.
4. Commit in one change: the new `deployments.json::prod` (real token catalog,
   stable feed ids, no `deepbook` block), all 8 service `network` flips, the
   endpoint/Pyth/Bluefin ids, `render-secrets.sh`, both Dockerfile ENTRYPOINTs,
   `start_checkpoint`, **`[vault_template]` omitted from the scheduler config**
   (§2.1), `price-charting` undeclared from compose and dropped from
   `nginx.prod.conf` + api-service's `derived_metrics_url` (§2.2), and the
   frontend's three mainnet id maps.
   *After this commit, `resolve_network.py` reports mainnet for prod and both
   testnet workflows refuse. That is the intended end state.*
5. `Deploy prod` `force_all`. `render-secrets.sh` now renders into the `mainnet`
   key slot; `Dockerfile.keeper`/`.scheduler` now pass `--network mainnet`.
6. Seed the real token catalog through auth-service + token-info's mutate port.
7. Run the FROST keygen ceremony for the first mainnet trading vault; back up
   the `hedge_signer_data` volume.
8. Vercel Production: `VITE_ENVIRONMENT=mainnet`, add `VITE_HEDGE_SIGNER_URL`,
   remove `VITE_CHARTS_URL`, redeploy.

### Phase 4 — verify before opening

1. `resolve_network.py prod` → `mainnet`. Confirm `redeploy-contract-testnet`
   and `wipe-provision-db-testnet` both **refuse** on prod, and that
   `publish-contract-mainnet` refuses on **staging**. Those three refusals are
   the acceptance test for the whole guard.
2. Indexer catches up from the publish checkpoint with no gaps.
3. One scheduler roll lands and creates **no** DeepBook pools; confirm the
   `"rolling buckets without pools"` log line and zero DEEP spent.
4. Gas-station sponsors a real mainnet tx.
5. Oracle-service serves stable feeds; keeper resolves `PriceInfoObject`s
   against mainnet Pyth.
6. **Full trading-vault deposit → appraisal → withdraw cycle with real funds, at
   minimum size, before anyone else's money is accepted** — including a round
   that sells nothing, since with no maker that is the expected case.
7. Confirm **no** `VaultCreated` event for the Move vault, and that the keeper's
   trading-vault pass runs while its Move-vault pass idles (§2.1).
8. Balance-monitor gauges + low-balance alerts firing against the real wallets.

### Phase 5 — open

Capped deposits first, at a number the team would cover out of pocket. Watch the
gas-station balance daily for the first week.

---

## 9. What I did not verify

- Mainnet Pyth / Wormhole package, state, and price-info-table ids (must come
  from Pyth's contract-addresses page, verified against a live mainnet fullnode
  — do not copy from memory).
- Mainnet Bluefin API base URLs.
- Real mainnet coin types and decimals for the launch assets.
- Whether the contracts compile clean against the mainnet Pyth branch — the
  `[dep-replacements]` default targets mainnet, so this is likely fine, but it
  has never been built for mainnet in CI.
- Whether `oracle-service`'s beta→stable feed mapping no-ops correctly when the
  source feeds are already stable.
- Whether any **frontend** surface hard-fails (rather than empty-states) with
  `DEEPBOOK_PACKAGE_ID === undefined` or `CHARTS_URL` unreachable. The backend
  consumers are verified (§2.2); the browser ones are not. Check the buy and
  chart screens before launch.
- Whether the trading-vault `deepbook-adapter` and `pool_allowlist` paths are
  inert without a DeepBook deployment — the scheduler's vault-allowlist branch
  (`main.rs:113`) is `Option`-guarded, but the adapter's own appraisal path was
  not traced. **This one matters more now**: with the Move vault deprecated, the
  trading vault is the product, and DeepBook is one of its venues. Confirm the
  curator flow is fully usable with no house DeepBook deployment recorded, or
  decide that trading vaults reference the *canonical mainnet* DeepBook
  (`contracts/vendor/deepbook/Published.toml` already pins it) while our option
  coins simply aren't listed on it.
- Whether anything besides `indexer/main.rs:70` hard-requires the `vault`
  package id, which would matter if the package is later dropped from the
  publish set entirely rather than published-but-unused.
