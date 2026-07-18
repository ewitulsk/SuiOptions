# Vault Curator Product — Implementation Decisions

**Scope**: the curated trading-vault product (epic SO-282), phases 1–6.
**Companion**: `docs/trading-vault/01-contract-design.md` is the design
spec; THIS document records the decisions made while implementing it —
defaults chosen, deviations from the spec, and the reasoning — so
reviewers and future work don't have to re-derive them from diffs.

**Deliverables** (all stacked on PR #289 → staging):

| Phase | PR | Ticket | Content |
|---|---|---|---|
| 1 | #289 | SO-283 | `contracts/trading-vault` core + `contracts/oracle-pyth` |
| 2 | #290 | SO-284 | `contracts/deepbook-adapter` + vault-core session/receive extensions |
| 3 | #291 | SO-285 | `contracts/options-adapter` (RFQ writer, calls + puts) |
| 4 | #292 | SO-286 | `trading_vault::vault_mm` + `options_core` recipient getter |
| 5–6 | (this branch) | SO-287/288 | indexer/api/keeper/gas-station/frontend |

---

## 1. Custody & session model

- **Curator can trade, never withdraw.** No vault function returns funds
  to the transaction sender. Funds leave free balances only through
  `Session` hot potatoes into allowlisted-adapter code, which by
  construction returns everything via `put`/`put_position`. The trust
  model is Morpho's: depositor security = vault-core invariants + the
  audit of each allowlisted adapter. (Move has no dynamic dispatch, so
  "generalizable to any protocol" *means* adapter packages + a registry
  entry.)
- **Three session flavors** (decision made during phase 2, extends the
  spec's two):
  - *Curator sessions* — cap-gated, full powers, Open + Closing.
  - *Force sessions* — permissionless, unlocked only when the vault is
    Closing or the queue head has aged past `unwind_grace_ms`; can never
    `take` balances. For disruptive-but-necessary unwind (cancel the
    book, sweep venue balances).
  - *Crank sessions* (new) — permissionless, ALWAYS available, same
    no-balance-take restriction. For non-discretionary maintenance whose
    outcome is fixed by prior state: settling a finished auction,
    redeeming an expired position, sweeping settled amounts. Adapters
    must expose only non-griefing entry points through them; that
    discipline is part of an adapter's audit surface.
- **Positions custody**: dynamic object fields tagged by adapter witness;
  only the tagging adapter can take, appraise, or receive them.
  `receive_position`/`receive_coin` (transfer-to-object sweeps) are
  witness-gated so junk transfers can never inflate `position_count` and
  wedge appraisals — unclaimed transfers just sit at the vault address.
- **`borrow_position`** (added in phase 2): immutable position reads for
  appraisals; mutation still requires a session.

## 2. Shares, fees, queue (defaults chosen)

- Ledger stakes with per-user cost basis; non-transferable. Genesis
  deposit mints 1:1; later deposits at NAV. **No seed-deposit
  requirement and no virtual-share offset**: with no donation path into
  the vault, the share-inflation attack has no lever (deviation from the
  design doc's "virtual offset + creator seed" — implemented simpler
  because the lever genuinely doesn't exist here).
- Crystallization at fulfillment: `value = shares × NAV/total`, `fee =
  10% of profit` (per-vault, capped by protocol config 30%), protocol
  takes `10% OF the curator fee` (Morpho-style), curator net auto-
  compounds as shares at the batch ratio (provably pps-neutral for
  remaining depositors — test-locked), protocol cut goes as cash to the
  core `Treasury`.
- Curator floor 5% (`min_curator_share_bps = 500`), enforced only on
  curator withdrawal requests while Open, protocol-disableable
  (`enforce_curator_share`). Curator-fee shares carry no lockup — the
  floor is the binding constraint.
- Queue: FIFO, all-or-nothing per request, crank stops at the first
  request it can't fund from free deposit-asset balance. Requests
  escrow shares + pro-rata basis at request time but crystallize at
  fulfillment (queued shares keep earning P&L).
- **Rotation keeps the old cap's stake as a cap-keyed claim ticket** (no
  conversion to an address stake — a creator-forced rotation can't know
  the holder's address). Old caps can exit their stake (no floor) but
  can't open sessions.

## 3. Appraisal / NAV

- `Appraisal` hot potato: seeds with the free deposit-asset balance,
  requires an attestation-priced entry for every other held asset type
  and an adapter appraisal for every custodied position. It snapshots
  (asset-type set, deposit balance, position count) at begin and aborts
  at consume if anything moved — same-PTB sessions can't skew NAV.
- Oracles are adapter packages minting `PriceAttestation`s
  (witness-allowlisted in `OracleRegistry`); core enforces only a
  `max_price_age_ms` backstop (default 60s). `oracle-pyth` wraps the
  battle-tested `spot_cross` math (feed pinning, staleness, confidence —
  all registry state, never caller args). Attestation prices are RAW
  smallest-unit ratios at 1e12 — decimals are the adapter's job.
- **Conservative marks everywhere**: DeepBook resting orders at locked
  cost; RFQ escrow at cost (premium upside never marked); written option
  positions at exercise-now (exercised range → strike proceeds,
  unexercised → min(spot, strike), computed with the buckets' own
  `required_settlement`/`required_collateral` math); held option coins
  at intrinsic. Undercounting NAV dilutes nobody who stays; premium
  mark-to-market is a later refinement.

## 4. DeepBook adapter

- **Wrapped, non-shared BalanceManager** — the load-bearing discovery.
  `new_with_custom_owner_caps_v2` is DeepbookAdminCap-gated (mainnet
  registry authorizes only DeepBook's own MarginApp), but
  `BalanceManager has key, store`: create it inside `init_custody`
  (curator is owner for that one tx), mint all three caps, wrap
  BM + caps into a `DeepBookCustody` held as a vault position. Owner
  paths become unreachable; trading uses
  `generate_proof_as_trader(&TradeCap)`, which validates the cap, not
  the sender, so it survives curator rotation. **Proven in Move tests
  against real DeepBook code** (order placed + cancelled on a real
  `Pool` via `create_pool_admin` test path) — the phase-2 testnet gate
  was satisfiable in-unit-test.
- Admin `PoolAllowlist` (curators trade vetted pools only) — the one
  structural brake retained after the no-guardrails decision.
- Custody tracks `assets` (manager balance types) and `active_pools`
  (pools that may hold locked balance); the custody appraisal must cover
  both sets, then records ONE value into the vault appraisal.
  `retire_pool` requires zero locked balance.
- Move borrow-checker note: `&mut custody.bm` alongside
  `&custody.trade_cap` in one call is rejected, and key objects can't be
  destructure-reassembled (`UID` must come from `object::new`) — the
  pattern that works is reference-pattern destructuring
  (`let DeepBookCustody { bm, trade_cap, .. } = custody`) inside small
  helpers.
- Dependency pinning: deepbookv3 manifests carry no published-at, so
  `[dep-replacements]` pins `deepbook` AND its `token` (DEEP) dep per
  env — testnet ids from `deployments.json`, mainnet from docs.sui.io.

## 5. Options adapter

- Drives the **generic `auction` package directly** with its own witness
  and the vault as `origin` — mirrors `options_vault`'s inlined pattern.
  Explicitly NOT via `options_rfq`, which `public_transfer`s outputs to
  addresses (wrong shape for a shared-object vault).
- Open RFQs are `RfqTicket` positions (escrow at cost) so value in a
  live auction stays in NAV. Settle is a crank: winner → write into the
  bucket, absorb `Position` + net premium, option coin to the bidder;
  no winner → escrow home; dead-bucket recovery refunds the best bid.
  Settle returns `Option<ID>` of the minted position for PTB chaining.
- Puts implemented as full twins (open/settle/expired/redeem/appraise)
  using `put_bucket::write_collateralized_balance` +
  `required_collateral`.

## 6. vault_mm (mm-bot on vault funds)

- Implements the standardized **3-argument** `release<T>` — mm-bot needs
  zero code changes (`collateral_account` = vault id, `release_module` =
  `"vault_mm"`).
- Authorization chain: core-minted `CollateralRequest` (signature/nonce
  verified) → `collateral_source == vault` → **`signer_token_recipient
  == the vault's own address`** → curator opt-in flag. The recipient
  check required an additive `options_core` getter
  (`collateral::signer_token_recipient`) — without it a curator could
  sign quotes routing the trade's outputs (Position + premium) to
  themselves. Test-locked ("theft quote" rejection).
- The 3-arg signature leaves no room for a registry parameter, so the
  kill switch is per-vault (`mm_release_enabled`, default OFF, curator
  toggled); sweeps/redeems/appraisals still require the `VaultMm`
  witness on the `IntegrationRegistry` (protocol-level kill for those).
- Outputs arrive by transfer to the vault address and are swept by
  cranks: Positions and **option coins as positions** (an option coin in
  free balances would be an unpriceable asset type and wedge every
  appraisal), plain premium coins into balances. Between release and
  sweep, NAV transiently under-counts by the in-flight amounts — keepers
  should sweep promptly (documented, accepted).

## 7. Backend (phase 5)

- Event pipeline: 24 new `ChainEvent` variants prefixed `Tv` (Move names
  collide with options_vault events; type-string dispatch disambiguates
  by package id, the Rust enum needs distinct names). `VecMap<TypeName,
  u64>` maps decode via a `TvTypeAmountMap { contents: [...] }` mirror.
- The trading-vault package families are **optional** in the indexer's
  `PackageIds` — deployments predating the product just don't subscribe
  (placeholder type strings that never match).
- Read model: `trading_vaults` + `trading_vault_positions` views
  (in-memory + Postgres + GraphQL + client + REST), share price derived
  event-side (`amount/shares` at deposit, `value/shares` at
  fulfillment, stored ×1e12). Stake-level data intentionally has no
  table — per-user history comes from the generic participant-filtered
  event feed.
- Keeper: a fulfillment pass for **cash-only vaults** (no positions, no
  foreign assets — the appraisal shape needing no attestation legs);
  appraisal-shape aborts (codes 82/83/78) classify benign. The shared
  `VaultProtocolConfig` id is not in `deployments.json`; it's recovered
  at boot from the package's publish-tx object changes (same trick the
  frontend uses). Full attestation-bearing keeper appraisals are the
  designated follow-up.
- Gas station: `trading_vault:*` templates — create / request_withdraw /
  enqueue_closed_stake as single anchored calls; deposit anchored on
  `begin_appraisal → deposit` with the oracle/adapter appraisal legs in
  the allowed set. Curator/session ops are NOT sponsored (bots pay their
  own gas).
- deployment-manager publishes all four new packages in order
  (trading-vault → oracle-pyth → deepbook-adapter → options-adapter);
  ids flow through `deployments.json` → token-info `/package-info` →
  every service, per the established pattern.

## 8. Frontend (phase 6)

- `/vaults` list + `/vaults/:id` detail screens: state/pps/TVL/queue
  metrics, positions (open + past), create-vault form, deposit +
  request-withdraw flows. Deposit PTBs are v1-limited to cash-only
  vaults (`positionCount == 0`) — composing attestation legs client-side
  is the follow-up; the button disables with an explanation otherwise.
- `VaultProtocolConfig` discovery client-side from the publish digest in
  `/package-info` (cached).

## 9. Known follow-ups (explicitly out of this pass)

1. Attestation-bearing appraisal composition (keeper + frontend) for
   vaults holding positions/foreign assets — the Move surface is
   complete; this is PTB-construction work.
2. On-chain (testnet) smoke of the wrapped-BM flow — proven in Move
   tests; a deployed-environment E2E belongs to the staging rollout.
3. Premium mark-to-market for option positions (conservative intrinsic
   marks today).
4. NAV time-series snapshots (devInspect cron) for richer performance
   charts; v1 charts can derive pps from deposit/fulfillment events.
5. DeepBook adapter: modify_order pass-through; per-order client-id
   conventions for bot bookkeeping.
6. Covered-call vault merge (per epic decision: later; migration seeds
   cost basis at migration-day NAV).
