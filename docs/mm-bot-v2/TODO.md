# mm-bot V2 / SO-299 — outstanding work

Status as of 2026-07-20, after PR #310 (5 commits: contracts,
indexer/api, keeper, hedge-signer, mm-bot V2 desk). Everything below is
what is NOT done, grouped by how blocking it is. Companion docs:
`00-plan.md` (strategy spec), `03-…`/`04-…` (venue custody plans),
`05-implemented-protocol-prerequisites.md` (what IS done + decisions).

---

## 1. Blocking a staging rollout (deploy/ops — no code on the PR)

- [ ] **Contract republish + activation.** options-core changed struct
  layouts, so the whole tree redeploys (publish order
  auction → core → rfq → vault → trading-vault → oracle-pyth →
  deepbook-adapter → options-adapter → **equity-oracle** → dbm-oracle
  when wanted). Then: allowlist `EquityOracle` (and `DbmOracle`) on the
  `OracleRegistry`, record ids in `deployments.json`, restart
  token-info. Note: deployment-manager **carries** the
  `equityOracle`/`dbmOracle` blocks forward but does not **publish**
  them yet — adding the publish + activation step to deployment-manager
  is itself a TODO (see §3.1).
- [ ] **Post-redeploy ritual** (existing conventions): `mm-bot
  deploy-collateral` + config id updates, scheduler
  pool-allowlist checks, indexer resubscribes automatically via
  token-info.
- [ ] **Provision the desk's trading vault**: create vault, curator cap
  into the bot wallet, `set_mm_release_enabled(true)`, fund custody,
  seed LP deposit; then flip `[desk] enabled = true` in mm-bot config.
  Until then the desk declines all RFQs by design.
- [ ] **Rollout sequencing around the teardown.** The mm-bot commit
  removes the legacy `onchain_swap`/`onchain_rfq`/`onchain_put_rfq`
  bidders — the covered-call vault's slice and proceeds-swap auctions
  have **no bidder** between deploying this mm-bot and either (a)
  provisioning the desk (replaces the RFQ side only) or (b) winding
  down options_vault rounds. The swap auctions have no successor at
  all (intentional per 00-plan Phase 0). Prod vaults wedge in settling
  without a swap bidder — sequence deliberately.
- [ ] **hedge-signer bring-up**: create AWS secret
  `options/<env>/hedge-signer` (`{"sui_key": "suiprivkey1…"}`),
  `terraform apply` the new ECR repo (mind the local-state drift —
  plan + `-target`), first deploy, then the **admin ceremony**:
  - construct the 2-of-2 multisig address from `GET /pubkey` + the
    curator key; decide 2-of-2 + backups vs 2-of-3 (open Phase-0
    decision from docs 03/04 — one answer for both venues);
  - `set_external_account(vault, multisig_addr, EquityOracle, budget_bps,
    daily_bps)`;
  - `equity_oracle::seed_equity` the vault's book entry;
  - `add_poster(keeper wallet)` on the `EquityBook`;
  - populate `[[vaults]]` in hedge-signer config (external account,
    vault address, margin package id, allowed pools, borrow caps).
- [ ] **Grafana**: confirm the new `alert_id`s route
  (`hedge-reconciliation`, `hedge-signer-denied`, `mm-desk-*`,
  `tx-failed-mm-bot-desk`) — rules are alert_id-generic but eyeball the
  contact points.

## 2. In-code `TODO(SO-299)` stubs (compile + are clearly marked)

- [ ] **Vault-held-coin exits** (`mm-bot desk/exits.rs`): resale and
  exercise of option coins in VAULT custody need a curator adapter
  entry point (take coin → sell/exercise → proceeds back in-session).
  Today only wallet-float coins exit; vault-held coins log as
  pending-adapter-support. This is the biggest functional stub — the
  desk's book is vault-custody-first.
- [ ] **Vault-funded auction bids** (`desk/auctions.rs`): on-chain RFQ
  bids escrow from the bot-wallet float (all outputs → vault). Strict
  vault custody needs an auction-bid adapter (escrow from vault
  balances via a session-compatible flow). Accepted gap, documented in
  module docs.
- [ ] **V2 written-position reconstruction** (`desk/book.rs`): the book
  rebuilds held coins/positions but not written-side inventory for the
  V2 netting engine.
- [ ] **Put-side exits** (`desk/exits.rs`): the exit ladder is
  call-complete; puts hold-to-expiry only.
- [ ] **Spread/scalp P&L attribution at fill detection**
  (`desk/book.rs`): funding + theta lines accrue; spread capture is
  attributed coarsely until fills are detected per-quote.
- [ ] **Multi-venue monitor aggregation** (`desk/monitors.rs`):
  delta-band/margin monitors assume one hedge venue (fine for paper).

## 3. Designed-for but deferred (follow-up tickets)

1. [ ] **deployment-manager publish step for equity-oracle/dbm-oracle**
   (+ activation PTB: witness allowlisting, `equity_oracle_objects`
   record with the EquityBook id — today the keeper discovers it from
   publish effects).
2. [ ] **Real venue equity readers** for the keeper's poster crank
   (`VenueEquitySource`): Bluefin REST (`GET /account` cross-checked
   vs on-chain IDS) and/or DBM on-chain read. Ships with
   `Disabled`/`Fixed` only. (For DBM the trustless path is the
   dbm-oracle in-PTB leg — the keeper composer currently supports only
   the `EquityOracle` witness; composing the DBM legs
   (manager/pool/margin-pool refs + pyth atts) is part of this item.)
3. [ ] **`HedgeVenue` real impls** (`desk/hedge.rs`): `deepbook_margin`
   first (borrow/sell/repay PTBs routed through hedge-signer, borrow-APR
   feed into the bid's hedge-cost term, risk-ratio monitor with
   emergency top-up fast-track), then `bluefin` (`bluefin-pro` SDK,
   authorized-wallet trading, funding feed). Only `paper` exists.
4. [ ] **FROST/Bluefin substrate for hedge-signer**: threshold-ed25519
   keygen + two-round ceremonies + Bluefin payload policy
   (login/authorize/withdraw). V1 is native-multisig/DBM posture only.
   Blocked on the Phase-0 asks to Bluefin (docs 03 §Phase 0).
5. [ ] **DBM Phase-0 spikes** (docs 04): testnet margin-pair reality
   check, multisig UX round-trip timing, gas posture for the multisig
   address.
6. [ ] **options-adapter spread-position gap**: `appraise_*_position`
   assumes pool-backed ranges; a custodied SPREAD position would
   misappraise. Gate or extend before the vault ever writes spreads.
7. [ ] **Put-side spread compression** (contracts): needs
   assignment-funded unwind fused into exercise; deliberately out of
   scope (calls-only per 00-plan).
8. [ ] **Premium mark-to-market** for option marks
   (`options_oracle::attest_*` internal upgrade; needs a
   vol-attestation design). Intrinsic-only today.
9. [ ] **Frontend**: vault external-account display (API fields exist:
   `external_account`, `external_exposure`, `latest_external_equity`),
   curator dashboard hedge panel (release/sweep flows driving the
   hedge-signer ceremony, positions/risk view), spot-trading tab.
10. [ ] **Pyth-leg gas-station templates** so attestation-bearing
    deposits are sponsorable (pre-existing follow-up; equity-record leg
    IS now allowed, pyth legs still aren't).
11. [ ] **Sim/desk soak on staging** vs the vault-sim reference before
    real-money parameters (00-plan sequencing: paper-hedged staging soak
    gates the real perps venue; venue mix decided empirically after).

## 4. Process / hygiene

- [ ] PR #310 review + **squash-and-merge** (repo convention), close
  SO-299. Consider splitting follow-up tickets from §3 at merge time.
- [ ] Pre-existing failures left untouched (not from this work):
  `deployments::tests::loads_the_repo_deployments_file` (TDEEP
  re-listed in the repo `deployments.json` — data assertion), clippy
  doc warnings in `crates/pricing/src/lib.rs`.
- [ ] Open product decisions still pending from 00-plan: epic
  structure (one epic vs split V1/V2), venue-mix decision timing.

---

**Shortest path to "V1 desk live on staging (paper-hedged)"**:
§1 republish → provision vault → hedge-signer ceremony (can trail; the
paper venue needs no external account) → flip `[desk]` on. No venue
integrations required.
