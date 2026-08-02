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
ships in this PR, and **the Vercel env var is now set** (production →
`https://sui-options.com/prod/oracle`, preview →
`https://sui-options.com/staging/oracle`). It takes effect on the next
frontend deploy — see §3.5, which is now a verification step rather than a
task.

### 0.3 Our Crossbar must serve the SAME queue Sui is on — RESOLVED, but verify

The queue ids are now resolved and in config (`[oracle]
switchboard_queue_id` / `switchboard_queue_key`), read from the
Switchboard `State` object on chain:

| Network | `Queue` object | `queue_key` |
|---|---|---|
| testnet | `0xe645d8979dac2fb901fb7c7b0ef3c9fad5dfaaf7ae2b0ce38a0b5ec63b819a99` | `0xc9477bfb5ff1012859f336cf98725680e7705ba2abece17188cfb28ca66ca5b0` |
| mainnet | `0x6e43354b8ea2dfad98eadb33db94dcc9b1175e70ee82e42abc605f6b7de9e910` | — |

**But this was more than a missing id.** `run_N` validates every signing
oracle against the `&Queue` object passed in, and the **public**
`crossbar.switchboard.xyz` signs under queue `86807068…`, while Sui
testnet's on-chain oracle queue is `c9477bfb…`. A bundle fetched from the
public instance would be rejected by every signature check, aborting
inside `run_N` with nothing useful in the error.

`QuoteBundle::require_queue` now catches that off chain and names both
queues. **What still needs verifying** is that *our* Crossbar — with your
paid Solana RPCs — answers for `c9477bfb…` and not the public queue.
That is §5.1, and it is a one-command check.

Nothing here breaks while `provider = "pyth"`.

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

- [x] `VITE_ORACLE_SERVICE_URL` is set on Vercel for **production**
      (`https://sui-options.com/prod/oracle`) and **preview**
      (`https://sui-options.com/staging/oracle`). The client appends
      `/descriptor`; the nginx route is an exact match on
      `/<env>/oracle/descriptor`.
- [ ] Redeploy the frontend so the build picks it up (env vars are baked at
      build time — an existing deployment will NOT pick this up on its own).
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

1. **Confirm our Crossbar serves Sui testnet's queue.** This is the one
   that matters (§0.3). From the host:

   ```
   curl -s "http://crossbar:8080/v2/update/4cd1cad962425681af07b9254b7d804de3ca3446fbfd1371bb258d2c75059812" \
     | jq -r '.oracleResponses[0].feedResponses[0].queue_pubkey'
   ```

   Must print `c9477bfb5ff1012859f336cf98725680e7705ba2abece17188cfb28ca66ca5b0`.
   If it prints `86807068…` it is still answering for the public/mainnet
   queue and **every quote will be rejected on chain** — the Solana RPC
   behind it is not the one backing Sui testnet's queue (Switchboard's
   non-mainnet queue lives on Solana **devnet**, per SO-333).

2. **Confirm `/oracles/sui` covers the signers.** Any signing oracle
   missing from that map is a hard client-side error, by design — dropping
   it would silently shrink the consensus set:

   ```
   curl -s http://crossbar:8080/oracles/sui | jq length
   ```

3. **Confirm the oracle set is ≤ 6.** On-chain only exposes
   `run_1..run_6`; a larger consensus set is refused client-side. Sui
   testnet's queue has `min_attestations = 3`, so 3–6 is the expected
   range.

4. **Wire `queue_id` into the payload assembly.** The config now carries
   it; `SwitchboardQuotePayload.queue_id` still needs to be populated from
   `[oracle] switchboard_queue_id` at the call site.

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
