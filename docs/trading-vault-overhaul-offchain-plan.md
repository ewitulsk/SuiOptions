# Trading-vault v2: off-chain integration map and delivery plan

Companion to [trading-vault-overhaul-plan.md](trading-vault-overhaul-plan.md)
(Revision 3, "the contract plan"). That document specifies the on-chain v2
package — tokenized `VaultPosition` NFTs, optional senior/junior tranches, the
capital risk-state machine, per-tranche queue lanes, junior generational reset,
curator escrow, and the terminal settlement pool. This document maps every
place the v1 protocol is integrated into `rust-backend/` and the frontends, and
lays out the plan to bring all of it to v2, including new UI features for
making tranching and NAV mechanics legible to new users.

Section 1 is the integration map (what exists, file-by-file). Section 2 is the
change plan by workstream. Section 3 is the new visualization feature set.
Section 4 is sequencing, testing, and rollout.

---

## 1. Integration map

### 1.1 What off-chain code depends on today (the v1 contract surface)

The v1 package (`contracts/trading-vault`) exposes ~45 public functions across
`vault.move`, `vault_mm.move`, `registry.move`, `price.move`; ~30 events in
`events.move` (+ `vault_mm::CollateralReleased`); and a large read path
(`stake_of`, `queue_head`/`queue_tail`/`queue_request`, `free_balance_of`,
`total_shares`, `pending_withdrawals`, …). Three off-chain facts shape
everything below:

- **There is no on-chain NAV getter.** NAV exists only inside the `Appraisal`
  hot potato and the `VaultAppraised` event; every service derives NAV from
  events or `dev_inspect`.
- **User claims are address-keyed** (`stakes: Table<StakeKey, Stake>`,
  `stake_of(vault, owner)`). Both the api-service and the frontend reconstruct
  a user's stake from this, and the api-service does it by *event replay*.
- **The withdrawal queue is one global FIFO** (`queue_head..queue_tail` over a
  `Table<u64, WithdrawRequest>`), and the keeper, frontend, and dashboard all
  walk it by deriving dynamic-field IDs from sequential u64 keys.

v2 deletes all three assumptions: positions are transferable NFTs, claims are
per-object, NAV splits through a waterfall, and the queue becomes two lanes
under one global sequence.

### 1.2 Rust backend — shared crates

| Location | What it does | v2 impact |
| --- | --- | --- |
| `crates/sui-tx/src/tx/trading_vault.rs` (449 ln) | Every vault PTB builder: `build_deposit`/`build_deposit_asset`, `build_request_withdraw` (pure u128 shares), `build_fulfill_withdrawals`, `build_fulfill_mixed` (begin/next/end potato chain over a single-queue `plan: &[(payout_type, count)]`), `build_crank_appraisal`, `build_enqueue_closed_stake`, `create_vault`, `set_mm_release_enabled`, `dev_inspect_free_balance` | **Full rewrite.** Deposit returns a `VaultPosition` NFT (must be transferred); withdraw consumes an object, not a share count; the fulfillment plan becomes lane-aware; `enqueue_closed_stake` is a deleted concept (settlement pool replaces it); `create_vault` spec gains the immutable `CapitalStructure` |
| `crates/sui-tx/src/tx/appraisal.rs` (1136 ln) | `discover_holdings` (chain reads: `asset_types`, external account, `PositionKey`/`PositionTagKey` dynamic-field walk) + `compose_appraisal`/`compose_switchboard_appraisal` (oracle legs → `begin_appraisal` → `appraise_balance` per asset → per-position adapter appraisal legs) | Survives structurally — appraisal stays total-NAV and tranche-agnostic per contract-plan §3.7 — but must parse the new vault layout in `discover_holdings` (TrancheBook, capital state, escrowed curator position) and verify the new `capital_mutation_seq` snapshot semantics |
| `crates/sui-tx/src/tx/template.rs` (gas-station allowlist, lines ~585–730) | Sponsored templates pinned to exact Move targets and arities: `trading_vault:deposit` (anchored on `begin_appraisal`+`deposit`), `create_vault`, `request_withdraw`, `amend_payout_asset`, `enqueue_closed_stake`; a large `appraisal_allowed` leg list | Every template re-anchored: deposit's NFT return means a `TransferObjects` command in the sponsored kind; `request_withdraw` switches from a pure arg to an owned-object input; drop `enqueue_closed_stake`; add split, merge, and settlement-pool-claim templates. Tests at 1417–1690 pin names |
| `crates/sui-tx/src/tx/admin.rs:218–275` | Ingress-pause PTB includes `trading_vault::registry::set_paused` | Retarget to v2 registry; otherwise unchanged |
| `crates/sui-tx/src/tx/test_tokens.rs:117–150` | `mint_and_deposit_into_vault` (mm-bot testnet seeding) | Update for NFT-returning deposit |
| `crates/protocol-types/src/events.rs:985–1650` | Rust mirrors of all 44 `Tv*` events + `IndexedEvent` envelope | New/changed structs for every position/tranche/capital event: `TvDeposited` gains `position_id`/`tranche`/`capital_generation`, `TvWithdrawRequested`/`Fulfilled` gain lane + `global_seq`, new `TvPositionSplit`/`Merged`, `TvCoverageBreach`/`Cured`, `TvImpaired`, `TvJuniorResetProposed`/`Cancelled`/`Executed`, `TvSettlementSnapshot`, `TvSettlementRedeemed`, curator-escrow events. Envelope format is stable |
| `crates/protocol-types/src/quote.rs` | Signed `Quote.release_module = "vault_mm"` — byte-exact BCS signature domain | Keep the module name `vault_mm` in v2 or all desk quotes re-sign; registry IDs are part of the signature domain, so a redeploy re-signs anyway (known: never cache registry ids) |
| `crates/indexer-graphql/src/lib.rs:180–231, 445–472` | `TradingVault` view struct (single `total_shares`, single `latest_pps_e12`) + hardcoded query strings | Extend with `capital_structure`, per-tranche shares/PPS/NAV, `senior_claim`, `risk_state`, `active_junior_generation`, lane heads; add a `vault_positions` query |
| `crates/deployments` + `crates/token-info-client` | `PackageInfo.tradingVault`, `TradingVaultObjectsInfo` (protocol config, integration/oracle registries, pool allowlist, equity/vol book, registrar pubkey, activation digest) | Add v2 object ids (unchanged list unless v2 introduces new shared objects); only `token-info` reads `deployments.json`, everyone else via snapshot — one place to extend |
| `crates/api-service-client:318–345` | `paused_vault_ids()` from `GET /trading-vaults` | Extend pause semantics to risk states (a `CoverageBreach` vault is quote-paused even if `deposits_paused == false`) |

### 1.3 Rust backend — services

| Service | Integration | v2 impact |
| --- | --- | --- |
| **keeper** (`src/trading_vault.rs`, 1839 ln) | The liveness crank: fulfillment (`queue_run` walks `queue_head..head+25` via derived df IDs; `fulfill` builds a single-queue mixed plan), `force_unwind_if_starved` (reads global queue head age), `refresh_marks` (`crank_appraisal` at `mark_refresh_interval_ms`), external-equity posting, RFQ/bid/redemption cranks, sweep of vault-addressed objects, benign-abort classification (codes 78/82/83/86…) | **Largest service rewrite.** Queue logic becomes per-lane ("oldest payable head across lanes"); the `tick` skip rule (`closed && pending == 0`) is wrong once closed vaults hold a perpetual settlement pool; new duties: risk-state transition cranking, reset-proposal monitoring/alerting (`alert_id` convention), terminal settlement snapshot crank, and an **appraisal-cadence SLO** — the hurdle accrual cap makes crank cadence a correctness obligation (contract plan §3.3/§9.4), not just freshness. Abort-code table renumbers |
| **indexer** | `event_types.rs` (44 `tv_*` type strings, `all_strings() -> [&str; 105]` fixed-size filter array, dispatch chain); `store/mod.rs` (single-tranche `TradingVaultState`, PPS computed from `TvDeposited`/`TvWithdrawFulfilled`); migrations 000011–000015; `graphql.rs` resolvers | New migration set: per-tranche columns on `trading_vaults` (senior/junior shares, NAV, PPS, `senior_claim`, `risk_state`, `impaired_since_ms`, generation, lane heads, settlement snapshot), a new **`vault_positions` table** (position_id, vault, owner, tranche, generation, shares, basis, lock, lineage, status: live/queued/consumed/settled), and lane-tagged withdraw request rows. Position *ownership* cannot come from events alone (transfers emit no vault event) — see §2.3 |
| **api-service** (`handlers/trading_vaults.rs`, 857 ln + `sui_rpc.rs`) | Routes: list, detail (+ live `BalanceKey<T>` GraphQL reads), `pps-history` (event replay, `SHARE_OFFSET` mirrored constant), **`stake/:address` (event-replay stake reconstruction)**, `trades`, `pending-requests` (seq-keyed replay, consumed by staging-mm-bot) | `stake/:address` is a deleted concept → replaced by `positions/:address` (JIT Sui GraphQL owned-object query by type, consistent with the stateless read-model pattern) + `positions/:positionId`. `pps-history` becomes per-tranche series with reset markers. `pending-requests` gains lane + global_seq. New endpoints: waterfall/state (`senior_claim`, buffer ratio, thresholds, risk state, reset proposal), settlement-pool status, position lineage |
| **mm-bot** (desk) | `desk/provision.rs` (create/adopt vault, `set_mm_release_enabled`), `desk/book.rs::reconstruct` (**NAV = pps × total_shares from indexer**, reservations ≤ NAV), `desk/exits.rs` (curator-session PTBs), `desk/auctions.rs` (vault-funded bids), `main.rs` `VaultRouting` in every signed quote | Provisioning gains the capital-structure spec + curator **escrowed commitment funding** (rotation/reset re-funding too). Quoting budget must switch from total NAV to the junior/risk-bearing measure and hard-stop on `CoverageBreach`/`Impaired`/`ResetPending` (quote sessions abort on-chain in those states — the bot must stop *before* burning gas). Health-gate interaction with deploys (known rollback trap) must tolerate the new states |
| **staging-mm-bot** | `src/vault.rs` (provision/wire direct-escrow custody, `add_quote_adapter`, `add_deposit_asset`), `main.rs::direct_funding_pass` + `direct_deposit` (Switchboard appraisal → deposit), `quote_budget` (free balance − buffer − pending obligations from `/pending-requests`) | Deposits become NFT-minting (bot must custody its own `VaultPosition`s, likely merging per tranche); `quote_budget` must subtract the senior claim dimension and respect risk state; `/pending-requests` shape change |
| **orderbook** | Direct-escrow settlement: vault abort-code constants (72/75/113), `SettleOutcome::VaultQuotingDisabled`, match submitters | Renumber abort codes; add a risk-state-gated outcome class so a breach reads as "vault risk-off", not an unknown abort |
| **hedge-signer** | `VaultResolver` pins `{pkg}::vault::TradingVault` type; `VaultPolicy` pins `vault::return_external`; FROST registration domain `tv_external_reg_v1` byte-identical to `vault::external_registration_message` | Repoint package/type pins; keep `return_external` name and the registration byte format in v2 (cheap to preserve, expensive to change — coordinate with contract §2.5). If v2 bumps the domain string, ceremony re-registration is required |
| **gas-station** | Builds `protocol_templates` from token-info snapshot | Inherits all template changes from `sui-tx/template.rs`; no local logic change |
| **oracle-service** | Serves `/descriptor` including `oracle_registry_id` (a trading-vault registry object) | Repoint id post-redeploy; no logic change |
| **balance-monitor** | Explicitly does not watch vault holdings today | Opportunity (not a break): watch `senior_claim` vs NAV and buffer ratio as first-class alerts |
| **token-info** | Serves `deployments.json` verbatim | Add v2 keys; hard cutover as usual |

### 1.4 Rust backend — tools

| Tool | Integration | v2 impact |
| --- | --- | --- |
| `tools/trading-vault-smoke` (1693 ln) | Full e2e: create → deposit → deepbook session → multi-asset → withdraw → fulfill → direct-escrow → fill_bid | **Rewrite as the v2 acceptance harness.** Add scenarios per contract-plan §6: tranche deposits/waterfall, lane fulfillment with a blocked junior head, breach/impairment/reset, split/merge/transfer, settlement-pool redemption |
| `tools/trading-vault-close` | initiate_close → unwind → `finalize_close`; also the stale-vault cleanup recipe | `finalize_close` becomes "trigger settlement snapshot"; the `total_shares ≤ max` rail becomes per-tranche. Also the tool that deletes the v1 staging vaults at launch |
| `tools/deployment-manager` (`trading_vault_init.rs`) | Publishes package, resolves registries, activation ceremony (allow adapters/oracles, registrar pubkey, posters) | Publish `vault_v2`; activation extends with protocol capital-structure floors/caps (hurdle cap, min junior thresholds, seasoning period) if configurable |
| `tools/exchange/roller.rs` | `allow_pools_for_vault` after rolls | Repoint package only |

### 1.5 Frontend (`frontend/`)

(Note: `screens/Vault.tsx`, `tx/vault.ts`, `api/vaults.ts`, `state/vault.ts`,
`VaultApyChart.tsx` are the deprecated covered-call product — not in scope.)

| Area | Files | v2 impact |
| --- | --- | --- |
| Routes/pages | `App.tsx` (`/vaults`, `/vaults/:vaultId`), `TradingVaults.tsx` (list + `CreateVaultCard`), `TradingVaultDetail.tsx` (1891 ln) | List: 3-state badge → 6-state; PPS/TVL columns split by tranche; create form gains capital-structure config. Detail: heaviest file in the overhaul (below). New route: `/vaults/:id/positions/:positionId` |
| **UserPanel** (`TradingVaultDetail.tsx:1523–1890`) | Deposit/withdraw tabs, `stake_of`-backed "Your shares / basis / value / lockup", fee preview, share-string max | **Total rewrite** — the single-stake-per-address assumption dies. Becomes a **position list** (N NFTs per wallet) with per-position value/basis/embedded-fee/lock, tranche selector on deposit, split-then-request partial withdrawal, merge, transfer with pre-transfer disclosure |
| Queue UI | `WithdrawQueueCard` + `AmendControl` (1410–1521), `useTradingVaultOnchain` (walks the queue table via derived df IDs, cap 50) | Two lanes with global sequence; per-lane head/tail walks; amend keyed by lane+seq; blocked-lane indicator |
| PTB builders | `tx/tradingVault.ts` (create/deposit/withdraw/amend/asset mgmt/external/spot targets), `tx/appraisal.ts` (1193 ln full appraisal planner+composer), `tx/exchangeAdapter.ts`, `tx/bluefinParent.ts` (`return_external`), `tx/admin.ts` (`registry::set_paused`) | Deposit handles the returned NFT (`transferObjects`) + tranche arg; withdraw takes the position object; new builders: `split_position`, `merge_positions`, settlement-pool redeem, (curator) escrow top-up/release, reset execution. The appraisal composer survives structurally — same total-NAV legs — with new-layout reads in `planAppraisal` |
| State/queries | `api/tradingVaults.ts` + `useTradingVaults.ts`: REST list/detail/pps-history/trades + **`stake/:address`**; gRPC reads of vault object, whitelist, BM; `useAppraisalPlan` | `TradingVaultStake` type → `VaultPosition[]`; wire types gain tranche/state/generation fields; `useAppraisalPlan` cache key adds capital state + generation; onchain queue hook re-keyed per lane |
| Sponsorship | `state/tradingVault.ts` decides sponsor per action; templates live in `sui-tx/template.rs` | Every sponsored flow re-verified against new templates; new sponsored ops: split, merge, settlement claim (users exiting a closed vault shouldn't need gas) |
| Charts | `TradingVaultPpsChart.tsx` (single PPS line) | Dual-series + annotations (see §3) |
| Naming hazard | `TradingVaultPosition` / `vaultHoldings.ts` "positions" = adapter *custody* holdings | Rename custody DTOs to `VaultHolding` before `VaultPosition` (the NFT) lands, or the codebase has two unrelated "position" types |
| Curator screens | `screens/curator/*` (Bluefin/FROST), `CuratorPanel` tabs | Mostly orthogonal; state gates widen from `state !== "open"` to the 6-state model; new curator ops tab (escrow commitment, reset, settlement) |

### 1.6 Dashboards

| App | Integration | v2 impact |
| --- | --- | --- |
| `dashboard/` (desk dashboard) | `api/vault.ts` — an **independent duplicate** of the vault DTOs; `api/indexer.ts` reconstructs the queue as `TvWithdrawRequested − TvWithdrawFulfilled` and LP P&L as `latest_nav − net deposits`; `PpsChart.tsx`; `deskState.ts` vault block | Update in lockstep or (better) fold onto the api-service DTOs to kill the second source of truth. Queue diff needs lane tags; LP P&L needs tranche attribution |
| `exchange-dashboard/` | `tx/routeFill.ts` vault-maker fill legs (ids come from orderbook service) | Low impact — settles via exchange-adapter against free balances |

---

## 2. Change plan by workstream

Ordering principle: **nothing here starts against a moving contract.** The
contract plan freezes the full v2 object layout, event schema, and entry-point
signatures before implementation (§7 step 2). Off-chain work begins from that
frozen interface; the event schema and error-code table are deliverables of the
freeze, not discoveries during integration.

### WS-0 · Shared foundations (blocks everything)

1. **Event schema v2** (`protocol-types/src/events.rs`): implement every new
   and changed struct from the frozen spec, versioned per contract-plan §5.
   Keep the `IndexedEvent` envelope. Golden BCS fixtures per event, captured
   from Move unit tests, checked in Rust round-trip tests (the gRPC JSON
   rendering traps make goldens non-negotiable).
2. **Deployments/token-info**: extend `TradingVaultObjectsInfo` and
   `deployments.json` with any new v2 shared objects; version the `terms_version`
   + spec hash from contract-plan §9.2 into the record so UIs can link exact
   terms.
3. **Error-code table**: one shared Rust constant module for v2 abort codes
   (today they're re-declared in orderbook, keeper, mm-bot). This is the moment
   to stop copy-pasting the numbers.

### WS-1 · sui-tx PTB layer

1. Rewrite `tx/trading_vault.rs` against v2: NFT-returning deposit (compose
   `TransferObjects` or hand the result to the caller for PTB composition, as
   the frontend needs), object-consuming `request_withdraw`, per-lane
   `build_fulfill_*` with a `(lane, payout_type, count)` plan, split/merge,
   curator-escrow ops, reset execution, settlement snapshot + redemption,
   `CreateVaultSpec` with `CapitalStructure`.
2. Update `tx/appraisal.rs::discover_holdings` for the v2 layout (TrancheBook,
   risk state, escrowed curator position appears as one more custodied object —
   confirm with contract whether it needs an appraisal leg; it should not,
   since it's shares not assets).
3. Rebuild `tx/template.rs` templates + tests: re-anchor deposit (with NFT
   transfer command), request_withdraw (object input), add split/merge/claim,
   delete `enqueue_closed_stake`. **Rule of the repo applies: any new frontend
   PTB shape needs a matching template or it silently isn't sponsored.**

### WS-2 · Indexer

1. Migration `000016+`: per-tranche vault columns, `vault_positions` table,
   lane-tagged requests, settlement snapshot table.
2. `event_types.rs`: new type strings, resized `all_strings` array, dispatch
   arms. `store/mod.rs`: apply-logic for the tranche book (senior/junior
   shares, claim, per-tranche PPS from `TvDeposited`/`TvWithdrawFulfilled`
   which now carry tranche ratios), risk-state transitions, generation
   rollover, settlement snapshot.
3. **Position ownership** (the one genuinely new indexing problem): transfers
   of a `key + store` NFT emit no package event. Decision:
   - *Source of truth for "who owns position X now"*: live Sui owned-object
     GraphQL query, served JIT by api-service (stateless pattern holds).
   - *Indexer role*: position **lifecycle and lineage** from events
     (mint/split/merge/consume/settle) — enough for supply-conservation
     invariant checks (Σ live+queued shares == outstanding, contract-plan §6)
     and for history, without chasing wallet-to-wallet transfers.
   - If later products need historical ownership (e.g., holder analytics), add
     checkpoint object-change ingestion for the one `VaultPosition` type — an
     additive follow-up, not v2-blocking.
4. Replay caution: deposit/write-style events double-count on checkpoint
   rewind (known staging trap) — the new apply-logic must stay idempotent per
   `(event, sequence)` like the existing tables.

### WS-3 · api-service

New/changed endpoints (all JIT per the stateless read-model pattern):

| Endpoint | Change |
| --- | --- |
| `GET /trading-vaults`, `/:id` | + capital structure, per-tranche NAV/PPS/shares, `senior_claim`, buffer ratio vs target/maintenance, risk state, generation, `impaired_since_ms`, reset proposal, settlement status |
| `GET /:id/pps-history` | Per-tranche series `{timestampMs, tranche, ppsE12, source}` with `reset` markers; keep total-NAV series for untranched |
| `GET /:id/stake/:address` | **Removed.** → `GET /:id/positions/:address` (owned-object query by v2 type + per-position estimated value/embedded fee from the latest tranche ratio) and `GET /positions/:positionId` (works for any holder — positions are transferable, the detail page must render one you just bought) |
| `GET /:id/pending-requests` | + `lane`, `global_seq`, `position_id`, payability flag ("junior lane blocked: coverage breach") |
| `GET /:id/waterfall` (new) | The §3.4a decomposition at latest appraisal: preferred, participation, senior NAV, junior NAV, accrued claim, mode — this single endpoint powers most of §3's visualizations |
| `GET /:id/settlement` (new) | Snapshot entitlements per tranche, redeemed vs outstanding claims |

### WS-4 · Keeper

1. **Lane-aware fulfillment**: reimplement `queue_run`/`fulfill` on
   "lowest global_seq among payable lane heads"; the planner must express
   "junior head unpayable, keep draining senior".
2. **Appraisal cadence as an SLO**: the hurdle accrual cap (contract §3.3)
   makes "crank at least every N" a correctness bound. Enforce
   `mark_refresh_interval_ms << accrual cap` in config validation and alert
   (`alert_id = "tv-accrual-cadence"`) if a vault's last consumed appraisal
   ages past a fraction of the cap.
3. **Risk-state duties**: crank state transitions when appraisals cross
   thresholds; alert on `CoverageBreach` (`alert_id = "tv-coverage-breach"`),
   `Impaired`, `JuniorResetProposed` (critical, per contract §8.5.3), reset
   execution/cancellation; monitor curator-commitment breaches.
4. **Terminal settlement**: permissionless snapshot crank on `Closed`; retire
   the `closed && pending == 0` skip (a settled vault with unredeemed claims
   still needs zero cranking, but the *snapshot itself* must be driven once).
5. Rebuild the benign-abort table from the v2 error module (WS-0.3); every tx
   failure keeps the `alert_id = "tx-failed-…"` convention at the handler.

### WS-5 · Market-making bots

1. **mm-bot desk**: provision v2 vaults with capital structure; fund and
   maintain the escrowed curator commitment (creation, rotation, post-reset);
   switch `book.rs::reconstruct` budget base from total NAV to the risk-state-
   aware measure (free balance is still the deploy constraint, but reservations
   should be bounded by junior/risk capital on tranched vaults); pre-check risk
   state before quote sessions and auction bids (`paused_vault_ids` extended to
   risk states); handle the deploy health-gate so a vault in breach doesn't
   roll back a deploy.
2. **staging-mm-bot**: NFT custody for its LP deposits (merge into one position
   per tranche per vault to bound object count); `quote_budget` respects lanes
   and risk state; adapt to the new `/pending-requests` shape.

### WS-6 · Orderbook, hedge-signer, gas-station, balance-monitor

- Orderbook: new abort codes + a first-class `VaultRiskOff` settle outcome.
- Hedge-signer: repoint type/package pins; **request the contract keep
  `vault::return_external` and the `tv_external_reg_v1` byte format** so FROST
  policy and completed ceremonies survive; otherwise schedule re-registration.
- Gas-station: config only (templates come from WS-1).
- Balance-monitor: add senior-claim-vs-NAV and buffer-ratio watches (new
  feature, low cost, high operational value).

### WS-7 · Tools

- `trading-vault-smoke`: rewrite as the v2 acceptance harness; add tranche,
  lane-block, breach/reset, split/merge/transfer, and settlement scenarios
  mirroring contract-plan §6's adversarial list. This is the release gate's
  "SDK behavior checked against the state-transition matrix" artifact (§9.5.3).
- `trading-vault-close`: v2 semantics (snapshot trigger) + the one-time job of
  deleting v1 staging vaults at launch (§5). Never touch the desk vault until
  its v2 successor is provisioned.
- `deployment-manager`: publish `vault_v2`, extend activation, record
  `terms_version` + spec hash in the manifest (§9.5.6).

### WS-8 · Frontend core

1. **Types & data layer**: regenerate `TradingVault*` wire/domain types
   (6-state union, tranche fields, `VaultPosition`), delete
   `TradingVaultStake`, rename custody "positions" to holdings, new hooks
   (`useVaultPositions(address)`, `useWaterfall(vaultId)`,
   `useQueueLanes(vaultId)`).
2. **Tx layer**: update `tx/tradingVault.ts` + `tx/appraisal.ts` per WS-1
   signatures; new builders for split/merge/transfer/claim; sponsorship map in
   `state/tradingVault.ts` extended (split/merge/claim sponsored; curator ops
   still not).
3. **Screens**:
   - List: state badges (6 states), per-tranche PPS/TVL columns for tranched
     vaults, create form with capital-structure config behind an
     "Advanced: tranches" section (immutable-at-creation warning, protocol
     floor/cap validation, linked `terms_version` disclosure per §9.3).
   - Detail: capital-state banner; per-tranche stat strip; positions panel
     replacing UserPanel's stake view (position cards: shares, basis, est.
     value, embedded fee, lock countdown, tranche, generation; actions:
     withdraw, split, merge, transfer-with-disclosure); lane-aware queue card;
     curator tab for escrow commitment status and reset flow.
   - New: position detail route (renders any position by id — required for
     secondary buyers doing due diligence pre-purchase), settlement claim view
     for closed vaults.
4. **Dashboards**: fold `dashboard/` vault DTOs onto api-service responses
   (kill the duplicate), lane-aware queue view, tranche-attributed LP P&L.

### WS-9 · Frontend visualization features → §3.

---

## 3. New features: making tranching and NAV legible

These are additive product features, not ports. They exist because senior/
junior mechanics are the least intuitive thing this protocol will ever ask a
new user to understand, and every input they need is already mandated by the
contract plan's read-API requirements (§5) — no extra on-chain surface.

Priority P1 items ship with v2; P2 fast-follow.

### 3.1 Waterfall explorer (P1) — the centerpiece

An interactive module on the tranched-vault detail page:

- A horizontal **capital-stack bar**: total NAV split into senior preferred
  (principal + accrued hurdle, visually separated), senior participation (in
  participating modes), and junior residual. Impairment renders the unfunded
  claim as a hatched "arrears" segment extending past NAV.
- A **what-if slider** on total NAV (e.g. −60%…+60% from current): dragging it
  re-runs the §3.4a waterfall client-side and animates the stack — the user
  *watches* junior absorb the first loss, watches senior only start losing
  after junior hits zero, and in participating modes watches senior's slice
  keep growing past the claim. Annotate the two break points (junior wiped;
  senior impaired) directly on the slider track.
- Mode-aware captions ("Preferred only: senior upside stops at its claim";
  "Capped participating: senior takes X% of residual up to Y% total return").
- Implementation: pure function of the `/waterfall` endpoint payload
  (mode, claim, principal basis, participation/cap bps, supplies); the same
  TypeScript waterfall function is property-tested against the Rust/Python
  reference model from contract-plan §6 so the UI can never disagree with the
  chain.

### 3.2 Dual-PPS chart with regime annotations (P1)

Extend `TradingVaultPpsChart` (Lightweight Charts already supports all of it):

- Senior and junior PPS as two series; total PPS retained for untranched.
- A dashed **hurdle reference line** (senior PPS if only the hurdle accrued) so
  the gap between senior-actual and senior-hurdle is visible at a glance.
- Background **regime shading** for `CoverageBreach`/`Impaired`/`ResetPending`
  windows, and vertical **markers** for junior generation resets (junior PPS
  re-bases to 1.0 — without a marker this looks like a rendering bug).
- Data: per-tranche `pps-history` + a state-transition series from the indexer.

### 3.3 Coverage gauge (P1)

A margin-style gauge for the junior buffer ratio: current
`junior NAV / total NAV` against the two immutable thresholds
(`maintenance_junior_bps` in red, `target_junior_bps` in amber, healthy in
green). Shown on both list rows (compact) and detail (full, with "what this
means" copy: below target = no new senior deposits; below maintenance =
risk-off, junior withdrawals paused). This single element answers "how safe is
senior right now" — the question the entire product hinges on.

### 3.4 Position NFT card + pre-trade disclosure (P1)

Each `VaultPosition` renders as a card (also the `Display` metadata story):
tranche badge, shares, current estimated value, **cost basis and embedded fee
liability** (est. fee if exited now), lock countdown, generation (with a
prominent "Wiped — permanently zero" treatment for stale generations),
lineage link (split-from/merged-from). The transfer flow interposes a
disclosure step showing exactly value-vs-basis — the contract plan requires
the UI to display both before a sale (§2.4), and the same card is what a
prospective secondary buyer sees at `/positions/:id`.

### 3.5 Queue lane visualizer (P1)

Replace the flat queue table with two lanes rendered side by side, each
request tagged with its global sequence; the "next to pay" head highlighted
across lanes; a blocked lane greyed with the block reason ("junior paused:
coverage breach") and the plain-language rule underneath ("senior keeps
flowing; junior resumes in original order when the breach cures"). Doubles as
the runbook view for the on-call.

### 3.6 Deposit preview with waterfall context (P1)

The deposit form, per tranche selected: shares to be minted at the locked
ratio, post-deposit buffer ratio (with a hard inline block when a senior
deposit would breach `target_junior_bps` — don't let the user sign a
guaranteed abort), the hurdle terms restated for senior, first-loss framing
restated for junior, and the linked `terms_version` disclosure. Deposit is
already sponsored; keep the whole flow gasless.

### 3.7 Lifecycle timeline & reset countdown (P2, but alert wiring in P1)

A vertical event timeline on the detail page (created → breach → cured →
impaired → reset proposed → executed → closing → settled) fed by indexer
state-transition events. During `ResetPending`, a prominent countdown card
with the proposal's recorded terms and the execution-time recomputation
caveat ("final required deposit is recomputed at execution"). The P1 subset is
the banner + alerting; the full timeline is P2.

### 3.8 Tranche education flow (P2)

A one-time "How tranches work" explainer (3–4 panels driven by the same
waterfall component in a canned scenario: deposit → gain → loss → junior
wipe), linked from every senior/junior badge. Content sourced from the §9.3
disclosures so product copy and legal copy can't drift apart. Candidate for
public-docs (GitBook) mirroring.

### 3.9 Settlement claim view (P2)

For `Closed` vaults: frozen per-share entitlement per tranche, your positions'
claimable totals, one-click sponsored redeem, and the vault-level
redeemed-vs-outstanding progress bar (the indexer must report unredeemed claim
totals per contract §8.7 anyway).

---

## 4. Sequencing, testing, rollout

### 4.1 Dependency order

```
Contract spec freeze (§7.2)  ──►  WS-0 foundations
                                   │
                     ┌─────────────┼───────────────┐
                     ▼             ▼               ▼
                WS-1 sui-tx    WS-2 indexer    (contract impl proceeds
                     │             │            in parallel on testnet)
        ┌────────────┤             ▼
        ▼            ▼         WS-3 api-service
   WS-4 keeper  WS-5 bots          │
        ▼            ▼             ▼
   WS-6 periphery (orderbook/signer/gas/monitor)
                     │
                     ▼
   WS-7 tools (smoke = acceptance gate)
                     │
                     ▼
   WS-8 frontend core  ──►  WS-9 visualizations (P1 in-release, P2 follow)
```

WS-1/WS-2 can start against the frozen interface with stubbed golden events
before the contract is code-complete; the smoke (WS-7) is what proves the two
sides agree.

### 4.2 Testing gates

- **Golden events**: BCS fixtures from Move tests, asserted in protocol-types.
- **Waterfall triple-implementation check**: Move ↔ Rust reference model ↔
  frontend TypeScript function, property-tested on shared cases cited by spec
  case ID (contract §9.5.2 extended to the UI).
- **Smoke matrix**: every §6 adversarial scenario that involves off-chain
  actors (lane block + senior flow, reset lifecycle, settlement redemption,
  sponsored deposit/withdraw/split/claim through the real gas-station).
- **Shadow indexing** on testnet before cutover (contract §7.6): run the v2
  indexer tables side-by-side and diff NAV/PPS against the reference model.

### 4.3 Staging rollout (delta to the 2026-08-11 redeploy runbook)

1. Pre-deploy: stop wallet-sharing services; snapshot/void the 18-stale-vault
   cleanup debt (they die with the v1 package — verify none matter first);
   never delete the desk vault until mm-bot has provisioned its v2 successor.
2. Publish `vault_v2` + activation; write `tradingVault`/`tradingVaultObjects`
   v2 records + `terms_version` to `deployments.json`.
3. Delete v1 vaults via the close tool per contract §5 (no migration path —
   confirmed: no live vaults).
4. Indexer: new migrations; **no checkpoint rewind across the cutover** (the
   v1 event stream is dead anyway); orderbook DB and desk vault re-provision
   follow the existing redeploy pattern (mm-bot deploy-collateral analog: the
   desk must fund its curator escrow before its health gate passes).
5. Force-all deploy; run the v2 smoke end-to-end; frontend env cutover last
   (frontend gates the whole /vaults UI on `TRADING_VAULT_PACKAGE_ID`, so it
   fails dark, not broken, during the gap).
6. Retire `contracts/trading-vault` from the tree once nothing references it
   (contract §5, revision 3).

### 4.4 Suggested ticket decomposition

One epic per workstream (WS-0…WS-9), tickets sized to the tables in §1 so each
file-level break in the map lands in exactly one ticket. The three highest-risk
tickets — sui-tx `trading_vault.rs` rewrite, keeper lane fulfillment, and the
frontend UserPanel→positions rewrite — should each be spiked against the
frozen interface before the rest of their epics are scheduled.
