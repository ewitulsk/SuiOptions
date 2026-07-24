# SO-299 deferred-work rollout — session handoff (2026-07-21 → 07-23)

Context document for continuing this work in a fresh session. Everything
below happened in one continuous effort: implement ALL eleven
"designed-for but deferred" items from `TODO.md` §3, deploy the whole
stack to staging, bring the V1 desk live, activate the DBM venue, and
debug the deposit/appraisal path end-to-end. The agent's persistent
memory (`~/.claude/projects/-Users-evanw--Dev-options/memory/`) carries
condensed versions of most of this — see `MEMORY.md` entries
"SO-299 deferred rollout state" and "Redeploy ops gotchas SO-299".

## 1. What was implemented (all merged to `staging`)

| TODO §3 item | Where | PR |
|---|---|---|
| 1. deployment-manager publishes + activates equity-oracle/dbm-oracle, records `equityBookId`/`volBookId` in deployments.json (`trading_vault_objects`) | tools/deployment-manager, crates/deployments, keeper | #311, #316 |
| 2. Venue equity readers — trustless `dbm_oracle::record{,_no_debt}` legs composed by the keeper (attested EquityBook path kept for Bluefin) | crates/sui-tx appraisal.rs, keeper `[external.dbm]` | #315, #328 |
| 3. `HedgeVenue::deepbook_margin` — dev-inspect manager reads, −borrow-APR funding feed, risk-ratio headroom, borrow/sell/repay/deposit PTBs via 2-of-2 co-sign ceremony | services/mm-bot desk/dbm.rs | #315 |
| 4. hedge-signer FROST substrate (two-round DKG, frost-ed25519 =3.0.0) + Bluefin payload policy (strict, fail-closed mirrors of bluefin-pro 1.13.0 payloads) | services/hedge-signer | #314 |
| 5. DBM Phase-0 spikes — testnet registry verified live; findings appended to doc 04 | docs commit on staging | — |
| 6. Spread-aware appraisal — core `range_overlaps_spread`/`spread_escrow_view`, vault_mm call+put spread marks, physical-appraisal gates (vault_mm codes 9/10, adapter 15) | contracts core/trading-vault/options-adapter | #312 |
| 7. Put-side spread compression — `put_bucket::{write_spread, exercise_spread, close_spread, redeem_spread_position}`; assignment-funded unwind fused into exercise; telescoped-ceil payout ledger (solvent by construction, dust-bounded); error codes 69/70; conservation tests incl. fractional-strike unit-chunk assignment | contracts/core | #312 |
| 8. Premium mark-to-market — `options_adapter::vol_book` (admin-seeded, keeper-posted, poster/interval/delta/ceiling guardrails, stale→intrinsic-only) + Brenner–Subrahmanyam extrinsic in `options_oracle::attest_*` (capped call≤spot, put≤strike); keeper vol crank off oracle-service realized vol | contracts/options-adapter, keeper, both appraisal builders | #312 |
| 9. Frontend — external-account display, curator hedge/spot tabs, client-side release_external appraisal, DBM equity leg (see §4) | frontend | #313, #329, #330 |
| 10. Pyth-leg gas-station templates (`[pyth]` config block; wormhole verify/auth-infos/update/potato-destroy + 0x1::option wrappers allowed under trading_vault:deposit; dbm_oracle record legs added later) | crates/sui-tx template.rs, gas-station | #311, #329 |
| 11. Soak — desk live paper-hedged vs the `[sim]` counterparty; monitors + alert routing verified | — | — |

Also merged en route: #317 (dbm-oracle testnet publish fix), #318 (desk
on), #319 (mm-bot INDEXER_GRAPHQL_URL), #320 (indexer canonical event
dispatch), #321 (vault repoint), #322/#323 (hedge-signer policy + DBM
venue config), #326–#328 (keeper dynamic-field fixes), keeper snapshot-race
fix. Jira: SO-300 (phase A), SO-301 (contracts wave), comments with PR
links on SO-299; the SO board exposes no "Done" transition via acli.

## 2. Live staging state (all ids current as of 2026-07-23)

- **Contract tree** redeployed (run 29812346542; deployments.json in repo
  is authoritative). trading_vault `0x0909ea47…eba0a2e` (note the leading
  zero — it mattered, see §3), options_adapter `0xbc2d415f…`, equity_oracle
  `0x9b36d540…`, dbm_oracle `0xc7c085f9…0491`, EquityBook `0x61b4968c…`,
  VolBook `0xe9a87e9c…`.
- **Desk vault (ACTIVE)** `0x70bb15b0046b8f6fd736b2cf58b178d84b8c2418925bbdd81caa5e74098d9a6d`
  (TUSDC, curator = bot wallet, ~1M TUSDC faucet seed, mm_release on,
  CuratorCap `0xd203aab3…3624`). First vault `0x31c0c534…4cd8` is dormant
  debris (its TvVaultCreated predates the indexer dispatch fix).
- **Vault contents**: one custodied position — a 0.1 TBTC call coin
  (`0x4e903c4a…e8c3`, CALL_0 of bucket `0x5be4c4d0…1910`) bought from
  Evan's frontend covered-call write for 6,947.46 TUSDC premium.
  pps ≈ 0.9984 (marked, not placeholder). One 1B-share withdrawal was
  fulfilled as the appraisal end-to-end proof.
- **DBM external account LIVE**: native 2-of-2 multisig
  `0xec548755761294d67138a047e6b82d1c19181ee2e6a8bb42be46b57635abc329`
  (members: bot wallet pubkey + hedge-signer service pubkey, threshold 2,
  ~1 SUI gas). MarginManager
  `0x3a525f86d2dfbdea49d2638f4c62f2c71482abcecf2551592aedb8e4421d366a`
  created BY the multisig via the full ceremony (curator `sui keytool
  sign` + hedge-signer POST /sign auto-tier + `multi-sig-combine-partial-sig`
  + `execute-signed-tx`). `set_external_account` pinned: DbmOracle
  witness, budget 2000 bps, daily 1000 bps. Desk runs `hedge_venues: 2`
  (paper + deepbook_margin).
- **Ceremony state**: VolBook + EquityBook posters = bot wallet; vols
  seeded TBTC 4500 / TSUI 7000 / TWAL 10000 / TDEEP 10000 bps; SUI +
  DBUSDC feeds seeded in the PythFeedRegistry (beta ids, 9/6 decimals).
- **Wallets**: staging deployer = keeper = mm-bot = curator =
  `0xab8d1b5a…4865` (in local sui CLI, alias elegant-ruby; keeper/mm-bot
  secrets hold the same key). hedge-signer service key addr
  `0xff5282d4…c02d` (AWS secret `options/staging/hedge-signer`, bech32).
- **Canonical testnet DBM ids** (docs/mm-bot-v2/04 Phase-0 findings):
  margin pkg latest `0xd6a42f4d…`, original `0xb8620c24…`, registry
  `0x48d7640d…`, SUI/DBUSDC pool `0x1c19362c…`, SUI margin pool
  `0xcdbbe6a7…`, DBUSDC margin pool `0xf08568da…`. Pyth testnet: pkg
  `0xabf837e9…`, price_info table `0xcb858b77…d3a7`, SUI/USD beta feed
  `0x50c67b3f…` → PriceInfoObject `0x1ebb295c…75a0`, USDC/USD beta
  `0x41f36259…` → `0x9c4dd400…9c81`.

## 3. Incidents root-caused during the rollout (don't re-learn these)

1. **Six redeploy attempts** — old deployments.json missing new fields
   (#316); deployer out of gas (faucet + `pay-all-sui` consolidation);
   dbm-oracle `PublishUpgradeMissingDependency` masked as a 60s RPC
   timeout — mainnet-branch Pyth resolves as unpublished on testnet and
   silently drops pyth/wormhole from the dep list (#317: mirror
   deepbook_margin's `[dep-replacements.testnet]`; its unit tests now
   need `sui move test --build-env mainnet`); gas-coin races from live
   services sharing the deployer wallet (stop scheduler/keeper/mm-bot
   first — sequentially, parallel dispatches cancel each other);
   hedge-signer secret was base64 not bech32 (health-gate rollback).
2. **Indexer dropped the whole Tv\* event family** — chain event types
   render short-form (`0x909ea…`) vs token-info's padded ids; raw string
   compare missed only the leading-zero trading_vault package. #320:
   dispatch on `to_canonical_string(true)`. The first vault predates the
   fix → replaced rather than replaying ~130k throttled checkpoints.
3. **Keeper silently disabled all vault cranks** after a same-wave deploy
   boot race (partial token-info snapshot → `build_ctx` silent None).
   Symptom: Evan's fill sat unswept; swept manually (cranks are
   permissionless). Fix: partial snapshot = fatal boot error → supervisor
   retries.
4. **The RPC provider (publicnode) serves NO dynamic-field index.**
   Struck four times: keeper price_info table resolution (pin the table
   id, #326), keeper per-feed + VolBook lookups (client-side
   `derive_dynamic_field_id` + plain object reads, #327), DBM leg
   attestations needed venue-asset feeds outside the token catalog
   (`[external.dbm] base_feed_id/quote_feed_id`, #328), and the frontend
   catalog price-info resolver (`price_info` is a dynamic OBJECT field —
   plain-field derivation yields a phantom id, the "Object 0xe512… not
   found" deposit error; #330 pins the table + derives entries, same as
   `tx/dbm.ts`).
5. **"Deploy staging" dispatches against HEAD at dispatch time** — one
   run shipped a pre-merge sha while reporting success. Always check the
   run's `headSha` equals the merge commit. Also: `deploy-image.yml` has
   never worked (startup_failure); use `gh run rerun <id> --failed` or
   "Deploy staging".
6. fullnode.testnet.sui.io is intermittently back for READS (serves
   dynamic fields; used it to mine PriceInfoObjects) but stays off the
   critical path. All workflows still need
   `rpc_url=https://sui-testnet-rpc.publicnode.com`.

## 4. Deposit path status (the last thing being tested)

Evan's frontend deposit on the DbmOracle-pinned desk vault progressed
through two fixes: the witness refusal (#329 added the client-side DBM
equity leg: full on-chain discovery — registry → owner→manager table →
pool config → PythConfig feeds → PriceInfoObjects — all via
`deriveDynamicFieldID` + `getObject`, verified live by
`frontend/scripts/dbm-discovery.ts`; gas-station allowlists the
`dbm_oracle::record*` legs), then the phantom-object error (#330, §3.4).
**As of session end: #330 merged, Vercel deploying, Evan had not yet
retried the deposit.** Next session: confirm the deposit lands; if it
fails again the error will name the next leg precisely. Withdrawals
already work end-to-end (keeper fulfillment proven: Pyth updates + 3
attests + VolBook call mark + `record_no_debt` + share pricing).

## 5. Outstanding work

- **Bluefin Phase-0 asks** (docs/03 §Phase 0.1): needs a human email to
  Bluefin. The FROST substrate + payload policy that depend on the
  answers are built and deployed but unused (no Bluefin account).
- **Soak** (§3.11): running paper-hedged; pass criteria per 00-plan
  (limit breaches, scalp+spread vs theta+funding, reconciliation quiet,
  restart replay) need days of wall-clock. Known calibration item: the
  bleed alarm (`mm-desk-bleed`) fires on an empty book (cost accrues
  with zero income). Venue mix (paper vs DBM vs Bluefin) decided
  empirically after.
- **DBM venue live-trading residuals** (from the #315 agent): real
  `adjust_to` may need fresh Pyth updates prepended (DBM's borrow-time
  oracle staleness check) and DeepBook lot-size rounding — untested
  because the desk's net delta has been 0.
- Put EXERCISE rung in desk exits still deferred (puts resale/offset,
  hold to expiry) — pre-existing §2 residual.
- Flash-exercise inert until `[desk.exits.spot_pools]` is configured.
- `[desk.v2]` (two-sided writing) ships disabled; the spread machinery
  (call + new put-side) is deployed but unexercised until v2 turns on.
- Prod: everything ships disabled/unprovisioned there; a prod bring-up
  repeats §2's ceremony (own vault, multisig, manager, config).
- Keeper's legacy covered-call vault resolution logs feed warnings for
  scheduler-rolled vaults whose configured feeds lack beta
  PriceInfoObjects — pre-existing scheduler feed-set question, untouched.
- Local-only: `social-bot` fails to LINK on this Mac (missing homebrew
  libpq path) — not a repo issue; CI builds fine.

## 6. Verification commands (quick re-orientation)

- Vault state: `GET https://sui-options.com/staging/api/trading-vaults/0x70bb…9a6d`
  (pps, positions, external_account, pending_withdrawals).
- Indexer view: `POST https://sui-options.com/staging/indexer/graphql`
  `{ tradingVaults { vaultId curatorCapId state } }`.
- Desk logs: SSM to `options-host` (i-0a8a3c28b87effa81, region
  us-east-1), `docker logs options-staging-mm-bot-1` — look for
  "desk started (vault-only maker)" with `hedge_venues: 2`; keeper logs
  for `alert_id` lines. Grafana via `gcx` (Loki datasource `loki`,
  labels `{env="staging", service="…"}`); the alert rule is fully
  alert_id-generic.
- Discovery sanity: `node frontend/scripts/dbm-discovery.ts` prints the
  whole DBM chain against staging.
