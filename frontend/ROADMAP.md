# Frontend Roadmap — Tideline (SuiOptions)

> **Deliverable**: this document, copied to `frontend/ROADMAP.md` once approved.
> Plan-mode constraints prevent writing to the repo until ExitPlanMode is accepted.

## Context

We just shipped a UI-only MVP of the Aqua design at `frontend/` (Vite + React 18 + TypeScript). All three screens render — Composer (Earn/Buy), Dashboard, Activity — but every data path is a hand-written mock in `frontend/src/mocks/`. Spot prices tick locally, MM quotes are simulated with `setTimeout`, positions are seed JSON, and "submitting" a quote runs a fake three-stage modal with no transaction.

The protocol it fronts already has substantial scaffolding on disk:

- **Contracts** (`contracts/sources/`) — Sui Move modules for `account`, `bucket`, `call_option`, `position`, `quote`, `treasury`, `events`, per `options-protocol-spec.md`.
- **Rust backend** (`rust-backend/crates/`) — `indexer` (tails Sui events, fans out over WS) and `quoting-service` (WS RFQ broker between retail + MMs).
- **Test tokens** (`test-tokens/`) — already deployed on testnet (per commit log `70de33f`).

This roadmap takes the frontend from "design demo" to "production tideline.fi on Sui mainnet" without re-doing UI work. Phases are sequenced **testnet-first**: each milestone is gated either on testnet (T-block) or mainnet (M-block).

---

## Target end-state

- Connect a Sui wallet → see your real Account balances, real bucket state, real positions, real on-chain history.
- RFQ a write or a buy → quotes stream from real MMs over WS → sign one → wallet submits a PTB → indexer confirms → UI updates.
- Exercise a held call, close-early via NFT transfer, redeem after expiry.
- Mobile-usable, monitored, deployed behind a CDN, with rollback.

---

## Scope note

**Frontend-centric.** Backend and contract gaps that block frontend integration are listed as **DEP-** items (dependencies, not owned by this roadmap). They reference §s of `options-protocol-spec.md`.

---

## Phase 0 — Foundations (parallel to everything else)

Not user-visible. Stops the project from going feral as it grows.

- [ ] **F0.1** ESLint + Prettier config; pre-commit hook (lint-staged + husky)
- [ ] **F0.2** Vitest + React Testing Library; first smoke tests on Composer/Dashboard/Activity render
- [ ] **F0.3** Playwright for E2E; one happy-path test (route between screens, open modal, close it)
- [ ] **F0.4** GitHub Actions CI: typecheck + lint + test + build on PR
- [ ] **F0.5** Env config pattern: `VITE_NETWORK`, `VITE_RPC_URL`, `VITE_QUOTING_WS_URL`, `VITE_INDEXER_WS_URL`, `VITE_PACKAGE_ID`, `VITE_PROTOCOL_CONFIG_ID`, `VITE_TREASURY_ID`. Wire `.env.testnet` / `.env.mainnet`. Validate at boot in `src/config.ts`.
- [ ] **F0.6** Replace hash routing with React Router v6. Routes: `/earn`, `/buy`, `/dashboard`, `/activity`, `/docs`, plus deep links like `/dashboard?tab=written`.
- [ ] **F0.7** Global state: TanStack Query for server state (RPC + WS-driven caches) + Zustand for transient UI state (modal stack, toast queue, wallet status).
- [ ] **F0.8** Error boundary at root; toast queue with stacking (current `Toast.tsx` is single-shot).
- [ ] **F0.9** Sentry (or equivalent) wired but disabled until M-block.

---

## Phase 1 — Wallet + read-only integration (T-block 1)

Goal: a user connects a wallet on testnet and sees their **real** account balances, positions, and history. No transactions yet.

### Wallet

- [ ] **F1.1** Adopt `@mysten/dapp-kit` + `@mysten/wallet-standard`. Drop in `WalletProvider` at root.
- [ ] **F1.2** Replace mock `connected: true` in `mocks/composer.ts` with `useCurrentAccount()`. Header `0x9f3a…42b1` now reflects the real address.
- [ ] **F1.3** Network switcher (testnet/devnet/mainnet) — gates on `VITE_NETWORK`. Warn-and-block when wallet chain ≠ app chain.

### RPC reads (Sui SDK)

- [ ] **F1.4** Spot price feed: replace local random walk with Pyth feed via `@pythnetwork/pyth-sui-js` (BTC/USD, SUI/USD). Wire into `mocks/composer.ts:useComposerState.spot` and `mocks/dashboard.ts:DASH_SPOTS`.
- [ ] **F1.5** Account fetch: `suiClient.getObject({ id: <Account ID> })` → balances map. Replace hardcoded `btcBalance = 0.4321`, `usdcBalance = 5000`.
- [ ] **F1.6** Bucket state fetch: read `Bucket<U,S>` shared object → `total_written`, `exercise_cursor`, `expiry_ms`, `strike`, `underlying_balance`. Feed `BucketBar` and `Tideline`.
- [ ] **F1.7** Held call tokens: `getOwnedObjects` filter by `CallOption<U,S>` type → group by bucket → owned positions in Dashboard.
- [ ] **F1.8** Owned Position NFTs: same pattern → written positions in Dashboard.

### Indexer WS

- [ ] **F1.9** Indexer client (`src/services/indexer.ts`). Connects to `VITE_INDEXER_WS_URL`, sends `Subscribe { after_sequence: 0 }`, streams `IndexedEvent` frames.
- [ ] **F1.10** Hydrate Activity screen from indexer events. Replace `ACTIVITY_SEED` in `mocks/activity.ts`. Map event types: `BucketCreated`, `WriteExecuted`, `Exercised`, `Redeemed`, `AccountDeposit`, `AccountWithdraw`.
- [ ] **F1.11** Live tail: append new events to the top of the timeline. The `header__status` "WSS live" dot now reflects real connection state.
- [ ] **F1.12** Reconnection w/ exponential backoff (1s → 30s), heartbeat (`Pong` reply to server `Ping`).

### Multi-bucket selection

The MVP hard-codes BTC · `JUN_26` · `USDC`. Real product must support all live buckets.

- [ ] **F1.13** Bucket discovery: indexer `BucketCreated` events → live bucket index. Power `BucketBar` selectors (asset, type, expiry, settlement).
- [ ] **F1.14** Asset/strike grid: derive `STRIKE_GRID` per (asset, expiry) from on-chain buckets rather than the hardcoded `[82000…95000]`.
- [ ] **F1.15** Multi-asset support: SUI is already in dashboard seed; promote to first-class in Composer.

### Loading + error UX

- [ ] **F1.16** Skeleton states for every screen (currently jumps from blank → data).
- [ ] **F1.17** Per-section error states ("can't reach indexer", "RPC timeout") with retry.
- [ ] **F1.18** Empty states polished (already partly there in Dashboard).

**Phase 1 exit criteria**: wallet on testnet → real balances + positions + activity render correctly. No tx submission yet.

---

## Phase 2 — Transaction flows (T-block 2)

Goal: every CTA in the UI submits a real Sui transaction and reflects the result.

### Quoting Service WS

- [ ] **F2.1** Quoting-service client (`src/services/quoting.ts`). Connect to `VITE_QUOTING_WS_URL`, send `RetailHello { role, version }`, handle `HelloAck`, `Ping/Pong`.
- [ ] **F2.2** `SubscribeBuckets` → stream `BucketUpdate` snapshots into TanStack Query cache. Replaces `mocks/composer.ts:useEffect` that ticks spot.
- [ ] **F2.3** `RFQRequest` flow: when user picks strike + amount in Composer, send `{ bucket_id, write_amount, side }`. Server returns `RFQResponse { quotes: RfqQuoteEntry[] }` after RFQ window (~2 s). Wire into `QuoteFeed.tsx`.
- [ ] **F2.4** Quote TTL countdown: render `valid_until_ms` as a live ticker on each quote row; auto-grey expired quotes.
- [ ] **F2.5** Replace `setTimeout`-faked submit with real flow (next bullets).

### PTB construction

- [ ] **F2.6** Move call builders in `src/chain/ptb.ts`:
  - `buildWritePtb({ bucket, mmAccount, signedQuote, underlyingCoin })` → `bucket::execute_write<U,S>` with `FlowKind::Writer`
  - `buildBuyPtb({ bucket, mmAccount, signedQuote, premiumCoin })` → same fn with `FlowKind::Trader`
  - `buildExercisePtb({ callOption, settlementPayment })` → `bucket::exercise<U,S>`
  - `buildRedeemPtb({ position })` → `bucket::redeem_position<U,S>`
  - `buildDepositPtb({ coin, kind })` / `buildWithdrawPtb({ amount, kind })`
- [ ] **F2.7** SignedQuote serialization: decode hex `signature` + decimal-string fields from `RFQResponse` into the `vector<u8>`/`u64` BCS shape the contract expects.
- [ ] **F2.8** Coin selection helper: pick or merge `Coin<T>` objects for exact amounts (covers `write_amount` underlying or `premium` settlement).

### Sign + broadcast

- [ ] **F2.9** Replace `ConfirmModal.tsx`'s mocked 3-stage flow with real `signAndExecuteTransactionBlock` calls. Stage transitions: `review → signing → broadcast → confirmed` driven by `useMutation`'s lifecycle.
- [ ] **F2.10** Same for `ActionModal.tsx` (exercise / claim / close-early).
- [ ] **F2.11** Post-tx state reconciliation: don't wait for indexer; optimistically update TanStack cache from the tx effects (object changes, events), let indexer correct later.
- [ ] **F2.12** Failure paths: `E_QUOTE_NONCE_USED`, `E_QUOTE_EXPIRED`, `E_INSUFFICIENT_BALANCE`, generic revert. Map to user-readable copy in `src/chain/errors.ts`.

### Deposit / withdraw

- [ ] **F2.13** New screen `/account` (or modal): list balances per asset, deposit/withdraw against the user's Account shared object. Currently no UI surface for this.
- [ ] **F2.14** First-time onboarding: if no Account object exists for the wallet, surface a "Create Account" CTA that calls `account::new`.

### Close early

- [ ] **F2.15** "Close early" CTA in `PositionCards.tsx:WrittenCard` currently routes to a mock buyback. Real flow: separate RFQ side (buyback ask from MMs) → wallet signs a transfer of the PositionNFT to the MM's address. Coordinate with quoting-service (DEP-B3).

**Phase 2 exit criteria**: all CTAs land real transactions on testnet, with correct optimistic + indexer-reconciled state.

---

## Phase 3 — Production polish (M-block)

Everything that has to be true before mainnet.

### UX safety

- [ ] **F3.1** Tx simulation preview (`dryRunTransactionBlock`) shown in confirm modals — actual gas, actual coin movements, side-by-side with the user's intent.
- [ ] **F3.2** Slippage / quote-drift guard: if a fresher quote arrives between user clicking and signing, block submission and re-prompt.
- [ ] **F3.3** Network mismatch guards (wallet on devnet, app on mainnet → big warning).
- [ ] **F3.4** Confirmation guards on destructive actions (close early, withdraw whole balance).

### Routing + sessions

- [ ] **F3.5** Deep links for every action (`/buy/btc-jun_26-85k?amount=0.05`).
- [ ] **F3.6** Persistent state across reloads: selected bucket, amount, recent activity.
- [ ] **F3.7** Multi-account / sub-account selector if a wallet holds >1 Sui account.

### Mobile + accessibility

- [ ] **F3.8** Responsive breakpoints. Today: `width=1280` fixed-viewport. Need ≥360px portrait. Dashboard cards stack; tideline collapses; modals go full-bleed.
- [ ] **F3.9** A11y pass: focus traps in modals, keyboard nav for tile selector, ARIA labels on icon-only buttons, color-contrast audit on Aqua palette (some `--aqua-ink-3` text on glass may fail AA).
- [ ] **F3.10** Reduced-motion respect (`prefers-reduced-motion`) — currently `aqua-tide`, `aqua-pulse`, `aqua-heartbeat` run unconditionally.

### Performance

- [ ] **F3.11** Route-level code splitting (`React.lazy` per screen).
- [ ] **F3.12** Memoize heavy lists (Activity timeline) once event log gets long; consider windowing (`@tanstack/react-virtual`).
- [ ] **F3.13** Asset budget: keep first paint < 200 KB gzip. Current bundle is 59 KB gzip JS / 7 KB gzip CSS — plenty of headroom but watch as wallet libs land.

### Observability + ops

- [ ] **F3.14** Sentry enabled w/ source maps, sampled at 10 %.
- [ ] **F3.15** Product analytics (PostHog or Plausible) — minimal: connect-wallet, RFQ-sent, quote-signed, tx-confirmed funnels.
- [ ] **F3.16** Status banner: subscribed to indexer + quoting-service health; "MM pool degraded" banner if quote latency spikes.

### Content + legal

- [ ] **F3.17** Docs page (currently a header stub). Glossary, write/buy walkthroughs, exercise mechanics, FAQ.
- [ ] **F3.18** ToS + Privacy + a (lawyer-blessed) risk disclaimer.
- [ ] **F3.19** Geo-gating if counsel requires it (Cloudflare-level, not React).

### Deployment

- [ ] **F3.20** Hosting on Cloudflare Pages or Vercel. Two environments: `testnet.tideline.fi`, `app.tideline.fi`.
- [ ] **F3.21** Preview deploys per PR.
- [ ] **F3.22** Rollback playbook.
- [ ] **F3.23** Custom domain + SSL.

**Phase 3 exit criteria**: mainnet deploy, full ops coverage, no P0 bugs in 7-day soak on testnet.

---

## Phase 4 — Post-launch growth (best effort)

- [ ] **F4.1** Closed-positions history tab in Dashboard.
- [ ] **F4.2** PnL chart over time (per asset, lifetime).
- [ ] **F4.3** Browser-push notifications: "you've been exercised", "your call is now ITM", "expiry tomorrow".
- [ ] **F4.4** Multi-quote PTB: split a large write across multiple MMs (spec §10 supports this; UI doesn't).
- [ ] **F4.5** Limit orders (post a resting writer order instead of RFQ). Requires backend support.
- [ ] **F4.6** Tax / portfolio export (CSV).
- [ ] **F4.7** Referral / rewards.
- [ ] **F4.8** i18n.
- [ ] **F4.9** Theme system (Aqua is the locked design, but `data-theme="aqua"` namespace makes Horizon/Lagoon swappable if desired).

---

## Backend + contract dependencies (DEP-)

Frontend integration is blocked on these. Each cites where the gap surfaces.

### Contracts (`contracts/sources/`)

- **DEP-C1** Quote nonce pruning (§3.6.4). Without a permissionless `prune_nonce`, MM Accounts accumulate dynamic fields unboundedly. Frontend can't fix; surfaces as RPC pagination pain.
- **DEP-C2** Confirm `FlowKind` is an explicit enum arg on `execute_write` (matches what F2.6 builds). Spec proposed inferring from empty Coin; implementation reportedly uses an enum.
- **DEP-C3** Settlement-asset abstraction. MVP UI assumes USDC. Contract supports per-bucket `settlement_type` but the UI's "settled in USDC" label needs to read it dynamically. Confirm read path on `Bucket`.
- **DEP-C4** Dust / minimum notional. Decide on-chain limits so UI can validate before submission rather than absorbing reverts.

### Quoting service (`rust-backend/crates/quoting-service/`)

- **DEP-B1** Buyback RFQ side for close-early (F2.15). Today RFQ is for opens only. Need symmetric "I want to sell this Position NFT" flow.
- **DEP-B2** Push retail-side account-state deltas. Today only MMs get `AccountStateUpdate`; retail polls RPC. Push would make balance updates instant.
- **DEP-B3** Documented WS error code catalog (we'll map in F2.12).

### Indexer (`rust-backend/crates/indexer/`)

- **DEP-I1** REST endpoint for paginated historical events. Today retail must replay the entire log over WS. Fine for testnet, will balloon on mainnet.
- **DEP-I2** Cursor-anchored subscribe (`Subscribe { after_sequence }` works; document sequence semantics so client reconnect can resume without dupes).
- **DEP-I3** Per-account event filter server-side (today the client filters all events).

### Pyth / oracle

- **DEP-O1** Confirm Pyth feed IDs and update cadence on Sui mainnet for each supported asset.

---

## Critical files this roadmap touches

Listed so an implementer can navigate quickly:

- `frontend/src/mocks/composer.ts` → split into `services/spotFeed.ts` + `services/quoting.ts`, keep `useComposerState` as the integration shim.
- `frontend/src/mocks/dashboard.ts` → split into `services/positions.ts` (RPC) + `services/buckets.ts` (RPC + indexer push) + the same `useDashboardState` shim.
- `frontend/src/mocks/activity.ts` → replace seed with `services/indexer.ts` consumer.
- `frontend/src/App.tsx` → swap hash router for React Router.
- `frontend/src/main.tsx` → add `WalletProvider`, `QueryClientProvider`, error boundary.
- `frontend/src/components/ConfirmModal.tsx`, `ActionModal.tsx` → wire to `useMutation` instead of `setTimeout`.
- New: `frontend/src/chain/ptb.ts`, `chain/errors.ts`, `chain/coins.ts`.
- New: `frontend/src/config.ts`, `.env.testnet`, `.env.mainnet`.
- New: `frontend/src/services/{indexer,quoting,spotFeed,positions,buckets}.ts`.

---

## Verification (per phase)

- **Phase 0**: `npm run typecheck && npm run lint && npm run test && npm run build` all green in CI on a PR.
- **Phase 1**: load testnet build, connect a funded wallet, see (a) real balances in the AmountInput, (b) real positions in Dashboard, (c) real on-chain events streaming into Activity within 5s of an indexer event.
- **Phase 2**: e2e: deposit testnet USDC → buy a call → exercise it → see the resulting Coin in the wallet. Repeat for writer flow: deposit BTC → write → wait or simulate exercise → claim. Each tx visible in Activity within 10s.
- **Phase 3**: lighthouse ≥ 90 perf/a11y on `/earn`. 7-day testnet soak with no P0 incidents. Mainnet deploy + smoke test.
- **Phase 4**: feature-flagged rollouts, monitored via PostHog funnels.

---

## Sequencing summary

```
P0 (foundations) ──┐
                   ├─► P1 (wallet + reads, testnet)
                   │       │
                   │       └─► P2 (txs, testnet)
                   │              │
                   │              └─► P3 (polish, mainnet)
                   │                     │
                   │                     └─► P4 (growth)
                   │
                   └─► (parallel: tests, CI, env config)
```

P0 runs alongside everything. DEP- items must close before the phase that needs them — surface each one as a tracked issue against the relevant repo crate.

---

## Open questions to resolve before kicking off P1

1. **Wallet library**: `@mysten/dapp-kit` is the obvious pick; confirm before pinning. Alternatives are Suiet's kit and rolling our own against `@mysten/wallet-standard`.
2. **State library**: TanStack Query + Zustand is the recommendation. Open to Jotai or Redux Toolkit if the team has a preference.
3. **Hosting**: Cloudflare Pages vs Vercel vs Netlify. CF Pages preferred for cost + geo-gating headers if needed.
4. **Single Aqua vs theme system**: roadmap assumes Aqua is the locked design. The CSS namespace (`[data-theme="aqua"]`) keeps Horizon/Lagoon swappable cheaply if desired.
5. **Mobile priority**: ship desktop-first for testnet, add mobile in P3? Or block testnet on mobile? Recommendation: desktop-first; mobile is M-block.
