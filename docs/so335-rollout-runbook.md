# SO-335 rollout runbook — oracle seam + Switchboard

Everything to do and everything to watch for when merging
[PR #381](https://github.com/ewitulsk/SuiOptions/pull/381) and redeploying
contracts + backend.

Plan and rationale: [`oracle-abstraction-plan.md`](oracle-abstraction-plan.md).

**This rollout does NOT switch providers.** It ships both adapters and the
machinery to switch. `provider = "pyth"` throughout, and the stack must
behave exactly as it does today when you are done. Flipping to Switchboard
is a separate, later action (§5) — treat "nothing changed" as the success
criterion here.

---

## 0. The three things most likely to bite

Read these before anything else; the rest of the document is ordering.

### 0.1 Seeded feed ids will NOT reach `/tokens` on their own

`deployments.json` now carries `switchboardFeedId` for all four tokens.
That is **not sufficient**. token-info serves a durable **DB** catalog, and
the `deployments.json`-derived overlay is read-time only with **"DB wins on
coin-type collision"** (`handlers/tokens.rs`). Existing rows will have
`switchboard_feed_id = NULL` after migration `000002`, and the overlay will
not override them.

The redeploy wipes only the **indexer** and **scheduler** DBs
(`redeploy-contract.yml`), so the token-info rows survive with a NULL.

**Consequence:** the catalog looks fine, Pyth keeps working, and the
Switchboard switch silently has nothing to price. Fix is §3.4 — a manual
`PUT` per token through the internal API.

### 0.2 The frontend needs a new env var, or it silently stays on Pyth

`tx/appraisal.ts` falls back to the compiled Pyth ids when it cannot fetch
the descriptor. That fallback is deliberate (it keeps this PR non-breaking)
but it means a missing `VITE_ORACLE_SERVICE_URL` produces **no error** —
just a frontend that will never follow a provider switch.

A scoped nginx route (`/{env}/oracle/descriptor` → `oracle-service:9013`)
ships in this PR. The Vercel env var does not; see §3.5.

### 0.3 The Switchboard `Queue` object id is still unresolved

`SwitchboardQuotePayload.queue_id` has no source yet — Crossbar's
`/v2/update` response does not carry the Sui `Queue` **object**, only a
`queue_pubkey`. Nothing breaks while `provider = "pyth"`, but the
Switchboard path cannot execute until this is supplied. See §5.1.

---

## 1. Pre-merge

- [ ] **Run the deploy-compiler gate.** This is the check that replaces the
      old CLI-version pin; it is the only thing that proves a redeploy will
      compile.
      ```
      cd rust-backend && cargo test -p deployment-manager --test deploy_build
      ```
      Expect: 2 passed. A failure here means **do not redeploy** — the
      publish would abort partway through the package sequence.
- [ ] `cargo test --workspace` — 114 targets.
- [ ] Squash-merge to `staging`.

---

## 2. Contract redeploy

### 2.1 This is a REDEPLOY, not an upgrade

Two independent reasons, either alone sufficient:

- `OracleRegistry` gained a `pins` field — a struct change.
- `oracle-pyth` now links Pyth's `sui-pro-compatible-contract-*` revision,
  which is a **different on-chain Pyth package**. Pyth's own guidance:
  *"There is no automatic upgrade path on Sui."*

### 2.2 What the publish does differently now

The sequence gains one package. `oracle-switchboard` publishes after
`oracle-pyth`, and activation now:

- allowlists **both** `PythOracle` and `SwitchboardOracle`, and
- seeds **both** `PythFeedRegistry` and `SwitchboardFeedRegistry` from the
  catalog, skipping tokens with no feed for that provider.

Both adapters are allowlisted deliberately, so switching later needs no
on-chain ceremony.

### 2.3 Standing pre-redeploy hygiene (unchanged, still required)

- [ ] **Stop wallet-sharing services first** — option-scheduler, market-sim,
      mm-bot. Concurrent txs from the deployer wallet lose object-version
      races mid-publish.
- [ ] **Deployer wallet needs DEEP** — scheduler rolls create DeepBook pools
      (500 DEEP each) and abort without it.
- [ ] **market-sim wallet needs DEEP + SUI** — refunded per redeploy.

### 2.4 Watch during the publish

- [ ] `deployments.json` gains `packageInfo.oracleSwitchboard` and
      `tradingVaultObjects.switchboardFeedRegistryId`. **If either is
      missing, stop** — activation partially applied.
- [ ] Activation logs `pyth_feeds` **and** `switchboard_feeds`. Both should
      be 4. A `switchboard_feeds = 0` means the catalog had no Switchboard
      keys at publish time (see §0.1) and the registry is empty.

---

## 3. Backend deploy

Order matters: token-info first (it is what every other service reads ids
from), then the rest.

### 3.1 token-info

- [ ] Migration `000002_switchboard_feed_id` runs automatically at boot
      (`run_migrations` in `main.rs`). Confirm it applied.
- [ ] Adds a nullable column only — no backfill, no downtime.

### 3.2 Everything else

- [ ] Standard force-all deploy. `oracle-service`, `gas-station` and
      `keeper` all carry changes.
- [ ] **`mm-bot deploy-collateral` + config id update is still mandatory**
      after any contract redeploy, or mm-bot's health gate rolls back the
      whole deploy set.

### 3.3 Verify the switch machinery answers — still on Pyth

```
curl -s http://<host>:9013/oracle/descriptor | jq
```

Expect `provider: "pyth"`, an `adapter` block with the **new**
`oracle-pyth` package id, and `feeds` covering all four coin types.

- [ ] `adapter` present. Absent means token-info is missing the adapter
      package or the governance objects — PTB composition is broken even
      though `/prices` still works.
- [ ] Also check `GET /prices/by-asset/<coin type>` returns a price.

### 3.4 Backfill the Switchboard feed ids into the DB catalog ⚠️

**This is the step §0.1 warns about.** Per token, against the **internal**
token-info port (9006, not the public 9005):

```
PUT /tokens/<coin_type>
{ …existing fields…, "switchboard_feed_id": "0x…" }
```

Verified hashes (WEIGHTED source — volume-weighted across venues):

| Token | Pair | Feed hash |
|---|---|---|
| TBTC | BTC/USD | `0x4cd1cad962425681af07b9254b7d804de3ca3446fbfd1371bb258d2c75059812` |
| TSUI | SUI/USD | `0x7ceef94f404e660925ea4b33353ff303effaf901f224bdee50df3a714c1299e9` |
| TUSDC | USDC/USD | `0x883ea8295f70ae506e894679d124196bb07064ea530cefd835b58c33a5ab6549` |
| TWAL | WAL/USD | `0x580de69fa5310460bead69dc3fd0c05988dea014d0e7c98aae22b67e7958fd9b` |

`PUT` is a full replace (`UpsertToken`), so **send every existing field** or
you will null out `pythFeedId` and break the live provider.

- [ ] Verify: `GET /tokens` shows both `pythFeedId` and
      `switchboardFeedId` on all four.

> On **prod** this is the only path — the test-token overlay is disabled
> there entirely, so the DB is the whole catalog.

### 3.5 Frontend

- [ ] Set `VITE_ORACLE_SERVICE_URL` in Vercel per environment:
      `https://sui-options.com/<env>/oracle`
      (the client appends `/descriptor`; the nginx route is an exact match
      on `/<env>/oracle/descriptor`).
- [ ] Redeploy the frontend.
- [ ] Verify in the browser: a `GET …/oracle/descriptor` returning
      `provider: "pyth"`. **No network call at all means the env var is
      unset** and the silent Pyth fallback is in play (§0.2).

### 3.6 nginx

- [ ] The new route ships in the deploy bundle. Confirm it reloaded —
      `curl https://sui-options.com/<env>/oracle/descriptor` should return
      JSON, not 404.

---

## 4. Post-deploy verification (still Pyth)

The bar is **"indistinguishable from before"**:

- [ ] Trading-vault deposit with a price attestation — exercises the whole
      seam (`emit_price_legs` → Pyth prefix → `oracle_pyth::attest`).
- [ ] Deposit is **sponsored** — proves the gas-station template rebuild
      did not drop the Pyth shape.
- [ ] Keeper appraisal / withdrawal fulfilment.
- [ ] mm-bot quoting, scheduler roll, market-sim banding.

Rollback is a redeploy of the previous contract set; nothing here is
one-way.

---

## 5. Switching to Switchboard (LATER — not part of this rollout)

### 5.1 Blockers to clear first

1. **Supply the `Queue` object id** (§0.3). Read it from the Switchboard
   `State` object — testnet
   `0x2086fdde07a8f4726a3fc72d6ef1021343a781d42de6541ca412cf50b4339ad6`.
2. **Make one live call through our own Crossbar.** The decoder in
   `switchboard-client` is pinned to a response captured from
   `crossbar.switchboard.xyz`; our instance is the same image but has never
   been exercised. Confirm `GET /v2/update/{hash}` and `GET /oracles/sui`
   both answer.
3. **Confirm the oracle set is ≤ 6.** On-chain only exposes
   `run_1..run_6`; a larger consensus set is refused client-side.

### 5.2 The switch

```
edit  services/oracle-service/config/config.<env>.toml
      [oracle] provider = "switchboard"
deploy oracle-service          # one service
```

Both adapters are already allowlisted, so **no on-chain transaction is
needed to switch on.**

### 5.3 Watch immediately after

- [ ] `/oracle/descriptor` reports `switchboard` with a populated `adapter`.
- [ ] **oracle-service boots at all.** It refuses to start if no catalog
      token has a Switchboard key — deliberate, so a half-seeded catalog
      cannot leave assets silently unpriceable. A boot loop here means §3.4
      did not take.
- [ ] A deposit composes and is sponsored.
- [ ] Compare a Switchboard price against Pyth for the same asset before
      trusting it with anything.

### 5.4 Retiring Pyth (later still)

Order is **not** reversible: `allow → verify → disallow`.

`disallow_oracle(PythOracle)` only after a soak. **Never disallow the last
allowlisted adapter** — that is not a pause, it is a freeze on withdrawals,
because fulfilment needs an appraisal too. The panic lever is the per-vault
pause.

---

## 6. Known-imperfect, deliberately

| Thing | Why it is fine for now |
|---|---|
| Phase 2a (Pyth pro endpoint auth) is unverified | `Authorization: Bearer` was already correct and the endpoint is config-driven; needs a live keyed-vs-unkeyed check, which was explicitly deprioritized |
| No CI runs `cargo test` | Pre-existing. `deploy_build.rs` is a local gate as a result — run it by hand (§1) |
| Descriptor exposes only `/oracle/descriptor` | Minimal surface; `/prices` and `/ws` stay internal. Widen if the browser needs them |
| Frontend falls back to compiled Pyth ids | Keeps this PR non-breaking, at the cost of failing silently (§0.2) |
| `oracle-service` not in nginx `depends_on` | Start-order only; nginx tolerates unreachable backends |
