# mm-bot V2 / SO-299 — outstanding work

Status as of 2026-07-20, after PR #310 (5 commits: contracts,
indexer/api, keeper, hedge-signer, mm-bot V2 desk). Everything below is
what is NOT done, grouped by how blocking it is. Companion docs:
`00-plan.md` (strategy spec), `03-…` (Bluefin custody plan),
`05-implemented-protocol-prerequisites.md` (what IS done + decisions),
`06-dbm-removal.md` (SO-334: what the DeepBook-Margin teardown removed and
what it costs).

---

## 1. Blocking a staging rollout (deploy/ops — no code on the PR)

- [ ] **Contract republish + activation.** options-core changed struct
  layouts, so the whole tree redeploys (publish order
  auction → core → rfq → vault → trading-vault → oracle-pyth →
  deepbook-adapter → options-adapter → **equity-oracle**). Then:
  allowlist `EquityOracle` on the `OracleRegistry`, record ids in
  `deployments.json`, restart token-info. deployment-manager publishes
  and activates the whole set (§3.1, done).
- [ ] **Post-redeploy ritual** (existing conventions): `mm-bot
  deploy-collateral` + config id updates, scheduler
  pool-allowlist checks, indexer resubscribes automatically via
  token-info.
- [ ] **Provision the desk's trading vault**: create vault, curator cap
  into the bot wallet, `set_mm_release_enabled(true)`, fund custody,
  seed LP deposit; then flip `[desk] enabled = true` in mm-bot config.
  Until then the desk declines all RFQs by design.
- [ ] **Hard cutover (decided 2026-07-20 — no phased sequencing).**
  Everything is testnet; deploy the whole stack at once. Consequence,
  accepted: the legacy `onchain_swap`/`onchain_rfq`/`onchain_put_rfq`
  bidders are gone, so any in-flight options_vault covered-call rounds
  lose their only auction bidder and will wedge in settling. As part of
  the cutover, don't leave them dangling: pause/`initiate_close` the
  legacy covered-call vaults (or just let dead rounds sit — testnet
  funds), and silence any keeper alerts they generate. The trading-vault
  desk is the successor product; options_vault merges into it later per
  the epic decision.
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

- [x] **Vault-held-coin exits** (`mm-bot desk/exits.rs`): DONE — resale
  of vault custody runs `vault_mm::release_coin_to_balances` +
  `deepbook_adapter::taker_swap_base_for_quote` in one curator PTB
  (min_out at model fair − concession); exercise runs
  `vault_mm::exercise_call_coin` (vault free settlement pays the
  strike; no flash fallback for vault coins); step 0 of the ladder
  nets written positions against same-bucket coin custody via
  `close_offset_position` (gated by `[desk.exits]
  offset_close_enabled`). The book now tracks VaultMm coin-custody
  positions per holding. Residual gap: vault FREE-BALANCE coins
  (auction-win redemptions) resale fine but have no exercise entry.
- [x] **Vault-funded auction bids** (`desk/auctions.rs`): DONE — bids
  place via `options_adapter::bid_on_auction` (escrow from vault
  balances, BidTicket into vault custody; keeper cranks burn tickets).
  Each live ticket's cost reserves NAV in the book; the reservation
  releases when the indexer position view shows the ticket burned.
  `max_concurrent_escrow` now caps total live-ticket cost. In-memory
  ledger only: a bidder restart drops the reservations (tickets still
  mark in NAV at cost via appraisal).
- [x] **V2 written-position reconstruction** (`desk/book.rs`): DONE —
  written inventory rebuilt from vault custody (indexer position ids +
  on-chain reads), same-bucket covered netting feeds true net greeks and
  the naked-short budget into the V2 gate.
- [ ] **Put-side exits** (`desk/exits.rs`): puts now resale (wallet +
  vault legs) and offset-close; put EXERCISE
  (`vault_mm::exercise_put_coin` exists on-chain but the desk rung) is
  still deferred — otherwise puts hold to expiry.
- [x] **Spread/scalp P&L attribution at fill detection**
  (`desk/book.rs`): DONE — cursor-persisted poller over indexer events
  (WriteExecuted + TvBidRedeemed⋈TvBidPlaced for auction wins), spread
  line at fair-at-detection, replay-safe across restarts.
- [x] **Multi-venue monitor aggregation** (`desk/monitors.rs`): DONE —
  `[[desk.hedge.venues]]` roster, summed shorts / min margin headroom /
  notional-weighted funding, per-venue labelled gauges; legacy
  single-venue config still parses.

## 3. Designed-for but deferred (follow-up tickets)

1. [x] **deployment-manager publish step for equity-oracle** — DONE.
   (The dbm-oracle half is gone with SO-334.)
2. [x] **Real venue equity readers** for the keeper's poster crank
   (`VenueEquitySource`): Bluefin REST reader landed with SO-305. The
   trustless DBM in-PTB leg was removed by SO-334 — the attested
   `EquityOracle` witness is the only equity path.
3. [ ] **`HedgeVenue` real impl** (`desk/hedge.rs`): `bluefin`
   (`bluefin-pro` SDK, authorized-wallet trading, funding feed into the
   bid's hedge-cost term, margin-headroom monitor). Only `paper` exists,
   and since SO-334 it is the only kind `venue_specs()` accepts —
   **the desk has no executable hedge until this lands.**
4. [ ] **Bluefin substrate wiring for hedge-signer**: the FROST
   threshold-ed25519 keygen, two-round ceremonies and payload policy
   (login/authorize/withdraw/sui_tx) are BUILT and deployed but unused —
   no Bluefin account exists. Blocked on the Phase-0 asks to Bluefin
   (docs 03 §Phase 0), which need a human email.
5. [ ] **options-adapter spread-position gap**: `appraise_*_position`
   assumes pool-backed ranges; a custodied SPREAD position would
   misappraise. Gate or extend before the vault ever writes spreads.
6. [ ] **Put-side spread compression** (contracts): needs
   assignment-funded unwind fused into exercise; deliberately out of
   scope (calls-only per 00-plan).
7. [ ] **Premium mark-to-market** for option marks
   (`options_oracle::attest_*` internal upgrade; needs a
   vol-attestation design). Intrinsic-only today.
8. [ ] **Frontend**: vault external-account display (API fields exist:
   `external_account`, `external_exposure`, `latest_external_equity`),
   curator dashboard hedge panel (release/sweep flows driving the
   hedge-signer ceremony, positions/risk view), spot-trading tab.
9. [ ] **Pyth-leg gas-station templates** so attestation-bearing
    deposits are sponsorable (pre-existing follow-up; equity-record leg
    IS now allowed, pyth legs still aren't).
10. [ ] **Sim/desk soak on staging** vs the backtester reference before
    real-money parameters (00-plan sequencing: paper-hedged staging soak
    gates the real perps venue; venue mix decided empirically after).

## 4. Process / hygiene

- [ ] PR #310 review + **squash-and-merge** (repo convention), close
  SO-299. Consider splitting follow-up tickets from §3 at merge time.
- [ ] Pre-existing failures left untouched (not from this work):
  ~~`deployments::tests::loads_the_repo_deployments_file` (TDEEP
  re-listed in the repo `deployments.json` — data assertion)~~ —
  **resolved by SO-317**, which strips TDEEP and deletes the two
  assertions. Worth knowing: because `cargo test` stops at the first
  failing binary, this one failure was truncating `cargo test
  --workspace` after ~17 of 68 test binaries while still printing a
  summary that read like a pass. Run it with `--no-fail-fast`.
  Still open: clippy doc warnings in `crates/pricing/src/lib.rs`.
- [ ] Open product decision still pending from 00-plan: epic structure
  (one epic vs split V1/V2). The venue-mix question is closed — SO-334
  removed DeepBook Margin, leaving Bluefin as the sole planned venue.

---

**Shortest path to "V1 desk live on staging (paper-hedged)"**:
§1 republish → provision vault → flip `[desk]` on. No venue integrations
and no external account required — the paper venue needs neither.
