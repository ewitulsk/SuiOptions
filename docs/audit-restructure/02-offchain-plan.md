# Audit Restructure — Off-Chain Plan

Companion to `01-onchain-plan.md`. The contract restructure (session scrap
+ four-package split + generic auction venue) touches every off-chain
consumer of the `options_protocol` package: PTB builders, gas-station
templates, the indexer's event decoding, mm-bot bidders, keeper cranks,
api-service, the frontend, and the deployment pipeline. This doc
inventories the blast radius and sequences the work so staging/prod never
run a mixed state.

## Inventory (what talks to the chain today)

| Consumer | Files | Talks to |
|---|---|---|
| sui-tx PTB builders | `crates/sui-tx/src/tx/{rfq,rfq_put,swap_auction,vault,vault_create,execute_write,execute_write_put}.rs` | `rfq::*`, `rfq_put::*`, `swap_auction::bid`, `vault::*`, `bucket`/`put_bucket` write paths |
| Gas-station templates | `crates/sui-tx/src/tx/template.rs` (`protocol_templates`) | vault fns + the whole `session_*` template block |
| mm-bot | `services/mm-bot/src/{onchain_rfq,onchain_put_rfq,onchain_swap}.rs` | `rfq::bid`, `rfq_put::bid`, `swap_auction::bid`; event `SwapRfqCreated`; reads `RfqAuction`/`SwapAuction` objects |
| keeper | `services/keeper/src/{submit,state}.rs` | `vault::{crank_redeem,settle_rfq,settle_rfq_expired,open_swap_rfq,settle_swap_rfq,finalize_round,open_rfq,select_bucket}`; events `RfqCreated`, `SwapRfqCreated` |
| option-scheduler | `services/option-scheduler/src/vault_roller.rs` | `vault::new_config`, `vault::create_vault` |
| indexer | `services/indexer/src/{event_types,worker}.rs`, `store/mod.rs`, `db/*` | decodes ~30 `{pkg}::events::*` type strings; materializes `rfqs`, `rfq_bids`, `vaults`, `vault_rounds`, `vault_user_receipts` |
| api-service | `handlers/{rfqs,vaults,pnl}.rs` + `crates/indexer-graphql` | rfq/vault views (reads indexer, not the chain) |
| frontend | `src/tx/{vault,session,composer,composer_put,dashboard,dashboard_put}.ts`; `src/session/*` | vault + bucket PTBs; the whole session login/custody stack |
| deployment-manager | `tools/deployment-manager/src/{deploy,main,lib,json_store}.rs` | single-package publish; `--deploy-session`; `deployments.json` schema |

## Phase 1 — Session scrap (pairs with on-chain PR 1)

1. **sui-tx `template.rs`**: delete the session template block —
   `session_open:*`, `session_revoke:*`, `session_account_create`,
   `session_write`/`session_buy` (+ put twins), `session_exercise`/
   `session_redeem`/`session_burn_expired` (+ put twins),
   `session_withdraw_with_root_sig[_eth]`, `session_deposit`,
   `session_fund:*`, `session_vault:*`, `session_deepbook:*`. Drop the
   `session`/`deepbook` params from `protocol_templates(…)` and fix
   callers (gas-station `main.rs`).
2. **frontend**: delete `src/session/` (`policy.ts`, `store.ts`,
   `accounts.ts`, `identity.ts`, `wallets.ts`) and `src/tx/session.ts`;
   remove session-login UI entry points and the `@yourorg/sui-siws-session`
   dependency + `SESSION_PACKAGE_ID` env.
3. **deployment-manager**: remove `--deploy-session` /
   `--session-contracts` flags, `publish_session_package`, and the
   `SessionTokensRecord` / `sessionTokens` field from the
   `deployments.json` schema (`json_store.rs`).
4. **deployments crate / token-info-client / gas-station**: drop
   `sessionTokens` plumbing wherever the deployment record flows.
5. **repo**: retire `session-tokens/` (the siws_session Move package and
   RFC) — archive or delete once nothing references it.
6. Verify: workspace builds, `grep -ri "session_\|siws" rust-backend/
   frontend/src` clean (modulo genuinely unrelated hits), gas-station
   boots and serves the remaining template set.

## Phase 2 — Deployment pipeline: multi-package publish

The single biggest structural change: one publish becomes four, in
dependency order, with address wiring between them.

1. `deploy.rs`: publish `options_core` → `auction` → `options_rfq` →
   `options_vault`. After each publish, write the published address into
   the dependents' manifests before compiling them. **Known gotcha** (see
   `.claude/move-type-normalization.md` history + contracts-publish-
   pipeline notes): the resolver reads a dependency's on-chain address
   only from that dep's own `Published.toml` — `[dep-replacements]`
   address overrides are silently ignored. The session-package publish
   already does the rewrite-manifest dance (`publish_session_package`
   rewrites Move.toml to `0x0` / deletes Published.toml); generalize that
   into the four-package loop, then delete the session variant.
2. `json_store.rs`: `PackageInfo{ packageId, … }` becomes per-package —
   e.g. `packages: { core, auction, rfq, vault }` each with
   `packageId`/`upgradeCapId`/`publishDigest`, plus the shared object ids
   (`adminCapId`, `protocolConfigId`, `treasuryId`) under core. This is a
   **breaking schema change**: token-info service (sole deployments.json
   reader) and token-info-client must move in lockstep; every other
   service gets addresses via token-info-client, so the cutover is
   centralized there.
3. Keep `--deploy-session` removal from Phase 1 merged first so the
   publish loop never has to consider it.

## Phase 3 — sui-tx rebuild (pairs with on-chain PR 3)

1. Replace `rfq.rs` / `rfq_put.rs` / `swap_auction.rs` with:
   - `auction.rs`: `create`, `bid`, `settle_swap`, `settle_expired`
     against the `auction` package.
   - `rfq_adapter.rs`: `create_call_auction`, `create_put_auction`,
     `settle_call`, `settle_put` against `options_rfq`.
2. `vault.rs` / `vault_create.rs`: same function names, new package id for
   the vault module; `open_rfq`/`settle_rfq`/`open_swap_rfq`/
   `settle_swap_rfq` PTB shapes change to the adapter/venue composition.
3. `execute_write*.rs`, `account.rs`: package id only (module paths
   unchanged inside core).
4. `template.rs`: retarget vault + write/exercise/redeem templates to the
   new package ids. Rule from the gas-station memory applies: every
   frontend PTB shape that changes needs its template updated in the same
   PR or it silently loses sponsorship.

## Phase 4 — Indexer

1. `event_types.rs`: rebuild the type-string table across four package
   ids. Renames:
   - `RfqCreated/RfqBid/RfqSettled/RfqExpiredUnsold` and the `PutRfq*`
     quartet → generic `auction::events::{AuctionCreated,AuctionBid,
     AuctionSettled,AuctionUnfilled}` **plus** `options_rfq::events::
     {OptionAuctionSettled,OptionAuctionUnfilled}` (carrying
     range/premium/fee).
   - `SwapRfq*` → the same generic `Auction*` events (swap mode is just an
     uncoupled auction now).
   - Vault + core events: same struct shapes, new package prefix.
2. `protocol_types::events` (`ChainEvent`): add the new variants; keep the
   materialized table shapes (`rfqs`, `rfq_bids`, `vaults`, …) stable —
   map generic + adapter events into the existing columns (an rfq row is
   now keyed by auction id; `origin` derives from coupling). Add an
   `auction_kind` (call/put/swap) column rather than new tables.
3. **Payload envelope caution** (indexer memory): `indexed_events.payload`
   stores the tagged `ChainEvent` envelope — GraphQL `payloadContains`
   filters nest under `payload`; renamed variants change those filter
   paths for any saved queries/dashboards.
4. Fresh deployment = fresh package ids, so no replay/migration of old
   events is needed; the new tables start empty on the audit deployment.

## Phase 5 — Services

1. **mm-bot**: collapse `onchain_rfq.rs` / `onchain_put_rfq.rs` /
   `onchain_swap.rs` toward one generic bidder over `auction::bid`
   (object shape is now one `Auction<E,B>` struct), with pricing per
   `auction_kind` from the adapter/creation events. Event subscription:
   `SwapRfqCreated` → `AuctionCreated`. Keep the `[onchain_swap]` bidder
   enabled in prod config (prod vaults wedge in settling without it).
2. **keeper**: `submit.rs` crank targets follow the new vault PTB shapes
   (finalize + adapter-consume composed in one PTB); `state.rs` discovery
   filters move to `AuctionCreated` (+ coupled-origin filter). Pyth
   prepend logic unchanged.
3. **option-scheduler** `vault_roller.rs`: package id only.
4. **tx-alerting convention** applies to every touched submit path:
   `error!(alert_id = "tx-failed-…")` at the service handler, benign
   race-losses suppressed (keeper Benign; mm-bot abort 31 → now the
   equivalent outbid/lost-race aborts of the new venue — re-derive the
   benign abort-code list from the new `auction` error module).

## Phase 6 — api-service / GraphQL

Mostly insulated (reads the indexer, not the chain): keep `GET /rfqs`,
`/rfqs/:id/bids`, `/vaults*` DTOs stable; surface `auction_kind`.
`indexer-graphql` client structs gain the same field. `pnl.rs` follows the
`ChainEvent` variant renames.

## Phase 7 — Frontend

1. `src/tx/vault.ts`, `composer*.ts`, `dashboard*.ts`: new package ids;
   vault flows unchanged in shape.
2. Any UI that surfaces RFQ/auction state moves to the generic auction
   fields (api-service DTOs keep this small).
3. Session deletions already landed in Phase 1.

## Sequencing & rollout

```
PR A  Phase 1 (session scrap, off-chain)          — anytime after on-chain PR 1
PR B  Phase 2 (deployment pipeline)               — after on-chain PR 3 merges
PR C  Phases 3-5 (sui-tx, indexer, services)      — stacked on PR B
PR D  Phases 6-7 (api, frontend)                  — stacked on PR C
```

- Staging keeps running the old deployment until the coordinated
  redeploy; nothing above breaks it at merge time.
- **The redeploy is the atomic point**: deploy the four packages, write
  the new deployments.json, then force-all deploy the services (per the
  redeploy-gotchas runbook: new ECR repos if any new service, prod
  Dockerfiles map prod→testnet, deploy.sh rolls back the whole set on one
  health-check failure).
- Scheduler rolls still need DEEP in the deployer wallet (5 DeepBook
  pools per roll) — unaffected by this work but part of the same redeploy
  checklist.

## Verification

- Golden cross-validation for the vault path (keeper ↔ contract accounting
  parity) re-run against the new packages — same harness that gated the
  original vault launch.
- E2E on staging: full covered-call round (roll → RFQ → mm-bot bid →
  settle → exercise → redeem → vault round finalize → proceeds swap) and
  the put twin, driven end-to-end with the indexer/api views asserted at
  each step.
- Gas-station: every remaining template exercised once against the fresh
  deployment (sponsorship dry-run), since template/PTB drift is silent.
- Alert-id sweep: intentionally fail one submit per service and confirm
  the `tx-failed-…` alert fires.
