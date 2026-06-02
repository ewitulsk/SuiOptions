# Jira Tickets — Earn go-live, Dashboard, Activity, Testnet faucet

> Source: planning thread (screenshot). These are written as Jira-style tickets
> but are **not** filed in Jira — they live here as implementation guides.
> Project: **SuiOptions (SO)**. File/line references are against
> `claude/jira-tickets-screenshot-tasks-UzXZE` at time of writing.

## Proposed epics

| Epic | Name | Covers |
|------|------|--------|
| **EPIC A** | Earn/Buy Composer — Go-Live | SO‑A1 (write wiring), SO‑A2 (scrub mocks), SO‑A3 (Buy "Coming Soon") |
| **EPIC B** | Account Surfaces — Dashboard & Activity | SO‑B1 (Dashboard finish), SO‑B2 (Activity live) |
| **EPIC C** | Testnet Developer Tooling | SO‑C1 (test-token faucet page) |
| **EPIC D** | Protocol / Contracts | SO‑D1 (the one contract change — needs detail) |

Suggested ordering / priority (from the thread): **SO‑A1 is #1**, then SO‑A2,
SO‑A3, then Dashboard/Activity, then the faucet page.

---

# SO‑A1 — Wire the Earn (writer) Composer to submit a real `execute_write` PTB

- **Epic:** EPIC A — Earn/Buy Composer — Go-Live
- **Type:** Story
- **Priority:** Highest (P0 — explicitly the #1 item)
- **Estimate:** 3 pts

## Summary
Replace the fake `submit()` flow in the Earn (writer) Composer with a real
Programmable Transaction Block that calls `bucket::execute_write` using the
MM's signed RFQ quote, so a connected writer can actually open a covered-call
position on chain.

## Current state
`frontend/src/mocks/composer.ts` → `submit()` is a stub: it just walks a
`setTimeout` state machine (`signing → broadcast → confirmed`) and shows a
toast. No transaction is built or signed:

```ts
const submit = () => {
  if (insufficient || !connected || quotes.length === 0) return;
  setConfirmStage("signing");
  setTimeout(() => setConfirmStage("broadcast"), 1100);
  setTimeout(() => { /* sets confirmSummary */ setConfirmStage("confirmed"); }, 2400);
};
```

Everything needed to build the real tx is already in scope:
- Selected bucket id: `selectedBucketId` (composer.ts).
- Live signed quotes: `rfqEntries` (`RfqQuoteEntry[]`) from `useRfq` — each has
  `quote` (all `execute_write` fields) + hex `signature` + `mm_id`. Shape:
  `frontend/src/api/quoting.ts`.
- Series coin types/decimals: `series.asset_coin_type`, `series.settlement_coin_type`,
  `series.asset_decimals` (`frontend/src/api/client.ts`).
- Deployment ids: `PACKAGE_ID`, `PROTOCOL_CONFIG_ID`, `TREASURY_ID` (`frontend/src/config.ts`).

## On-chain target (do not change)
`contracts/sources/bucket.move:125`

```move
public fun execute_write<Underlying, Settlement>(
    bucket: &mut Bucket<Underlying, Settlement>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    signer_account: &mut Account,
    underlying_in: Coin<Underlying>,
    premium_in: Coin<Settlement>,
    flow: FlowKind,
    position_recipient: address,
    call_token_recipient: address,
    signed_quote: SignedQuote,
    clock: &Clock,
    ctx: &mut TxContext,
)
```

Writer-flow invariants enforced by `execute_write_with_quote`
(`bucket.move:217`, `FlowKind::Writer`):
- `signer_recipient == call_token_recipient` (the MM/buyer receives the call tokens).
- `premium_in.value() == 0` (writer supplies a **zero** settlement coin).
- `underlying_in.value() == write_amount` (writer supplies exactly the quote's underlying).
- Writer (the executor/`ctx.sender()`) receives the net premium and the `Position` NFT.

Quote/flow constructors:
- `quote::new_quote(protocol_id, signer_account_id, signer_token_recipient, bucket_id, write_amount, premium, valid_until_ms, nonce)` (`quote.move:38`)
- `quote::new_signed_quote(quote, signature)` (`quote.move:60`)
- `bucket::writer_flow()` / `bucket::trader_flow()` (`bucket.move:76,78`)

## Implementation guide

### 1. New PTB builder — `frontend/src/tx/composer.ts` (new file)
Mirror the doc-comment + `requirePackage()` style of `tx/admin.ts` and
`tx/dashboard.ts`. Add a `buildWriteTx` for the writer flow:

```ts
import { Transaction, coinWithBalance } from "@mysten/sui/transactions";
import { SUI_CLOCK_OBJECT_ID, fromHEX } from "@mysten/sui/utils";
import { ENV, PACKAGE_ID, PROTOCOL_CONFIG_ID, TREASURY_ID } from "../config";
import type { RfqQuoteEntry } from "../api/quoting";

export type WriteParams = {
  entry: RfqQuoteEntry;          // chosen MM quote (default: quotes[0], the best)
  underlyingCoinType: string;    // series.asset_coin_type
  settlementCoinType: string;    // series.settlement_coin_type
  writer: string;                // connected wallet (position_recipient)
};

export function buildWriteTx(p: WriteParams): Transaction {
  const pkg = requirePackage();
  if (!PROTOCOL_CONFIG_ID || !TREASURY_ID) throw new Error("missing config/treasury id for env");
  const q = p.entry.quote;
  const tx = new Transaction();

  // Reconstruct the signed quote on-chain (Quote/SignedQuote are Move structs,
  // not pure args). hex fields → vector<u8>.
  const quoteArg = tx.moveCall({
    target: `${pkg}::quote::new_quote`,
    arguments: [
      tx.pure.vector("u8", Array.from(fromHEX(strip0x(q.protocol_id)))),
      tx.pure.id(q.signer_account_id),
      tx.pure.address(q.signer_token_recipient),
      tx.pure.id(q.bucket_id),
      tx.pure.u64(BigInt(q.write_amount)),
      tx.pure.u64(BigInt(q.premium)),
      tx.pure.u64(BigInt(q.valid_until_ms)),
      tx.pure.u64(BigInt(q.nonce)),
    ],
  });
  const signedQuote = tx.moveCall({
    target: `${pkg}::quote::new_signed_quote`,
    arguments: [quoteArg, tx.pure.vector("u8", Array.from(fromHEX(strip0x(p.entry.signature))))],
  });

  const flow = tx.moveCall({ target: `${pkg}::bucket::writer_flow` });

  // Writer supplies exactly write_amount of underlying; premium side is a zero coin.
  const underlying = tx.add(
    coinWithBalance({ balance: BigInt(q.write_amount), type: p.underlyingCoinType }),
  );
  const premiumZero = tx.moveCall({
    target: "0x2::coin::zero",
    typeArguments: [p.settlementCoinType],
  });

  tx.moveCall({
    target: `${pkg}::bucket::execute_write`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType],
    arguments: [
      tx.object(q.bucket_id),
      tx.object(PROTOCOL_CONFIG_ID),
      tx.object(TREASURY_ID),
      tx.object(q.signer_account_id), // MM Account (shared, mutable)
      underlying,
      premiumZero,
      flow,
      tx.pure.address(p.writer),               // position_recipient = the writer
      tx.pure.address(q.signer_token_recipient), // call_token_recipient = the MM/buyer
      signedQuote,
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
  return tx;
}
```

Notes / things to verify while building:
- **`signer_account` shared?** Confirm `Account` (`contracts/sources/account.move`)
  is a shared object so `tx.object(signer_account_id)` resolves with the right
  `initial_shared_version`/mutability. If it's owned by the MM, this needs the
  quoting/account layer to expose it as shared — flag in the PR.
- `strip0x` is a 2-line helper (`s.startsWith("0x") ? s.slice(2) : s`).
- Keep `requirePackage()` identical to the other `tx/*.ts` files (throw the same
  "No deployment for VITE_ENVIRONMENT=…" message).
- For now only the **writer** flow is needed (Buy/trader is being gated behind
  "Coming Soon" in SO‑A3). Leave a `trader_flow` builder out until Buy ships.

### 2. Make `submit()` real — `frontend/src/mocks/composer.ts`
- Import `useSignAndExecuteTransaction` from `@mysten/dapp-kit` and `buildWriteTx`.
- Pick the chosen entry: `const bestEntry = rfqEntries[0]` (already best-price-first).
- Replace the `setTimeout` body. Follow the exact idiom in
  `state/dashboard.ts:submit` (review-captured modal, `signing → broadcast →
  confirmed`, `try/catch` with a `failed · <message>` toast):

```ts
const { mutateAsync: signAndExecute } = useSignAndExecuteTransaction();

const submit = async () => {
  if (insufficient || !connected || rfqEntries.length === 0 || !series || !account) return;
  const entry = rfqEntries[0];
  setConfirmStage("signing");
  try {
    const tx = buildWriteTx({
      entry,
      underlyingCoinType: series.asset_coin_type,
      settlementCoinType: series.settlement_coin_type,
      writer: account.address,
    });
    setConfirmStage("broadcast");
    await signAndExecute({ transaction: tx });
    // build confirmSummary from `entry.quote` + series (real values, not mock)
    setConfirmStage("confirmed");
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    setToast(`failed · ${message}`);
    setTimeout(() => setToast(null), 6000);
    setConfirmStage(null);
  }
};
```
- Build `confirmSummary` from the executed quote: `premium = Number(entry.quote.premium)/settlementScale`,
  `amount` from `entry.quote.write_amount / 10^asset_decimals`, `strike` from
  `selected.strike`, `asset/expiry` from `series`. Drop the mocked `bucket.cursor`
  range math (or keep cursor display until SO‑A2 lands live cursors).
- On `closeConfirm`, after confirmed, invalidate the buckets/positions queries so
  Dashboard reflects the new position: call `useQueryClient().invalidateQueries({ queryKey: ["buckets"] })`
  and `["positions", address]`.

### 3. CTA / disabled state
`Composer.tsx`'s CTA already disables on `!connected || insufficient ||
quotes.length === 0 || bucketsLoading || bucketsEmpty`. No change needed beyond
making the click async. Ensure the button is disabled while `confirmStage` is
`signing`/`broadcast` to prevent double-submit.

## Acceptance criteria
- [ ] A connected wallet holding ≥ `write_amount` of the underlying can select a
      bucket/strike/amount on **/earn**, get a live quote, click the CTA, sign in
      their wallet, and land a successful `execute_write` tx on testnet.
- [ ] After confirmation, the new `Position` appears under **Dashboard → Calls
      written** (no manual refresh).
- [ ] A revert (expired quote, invalidated bucket, insufficient balance) surfaces
      the chain error message via the toast and resets `confirmStage` to `null`.
- [ ] No `setTimeout`-driven fake stages remain in `submit()`.
- [ ] `npm run typecheck` passes.

## Testing / verification
- Manual: testnet wallet with TBTC (mint via SO‑C1), run `npm run dev`, write a
  TBTC call, confirm the tx digest on a Sui explorer and the Position on Dashboard.
- Confirm the writer received net premium (gross − fee) and the MM/buyer received
  the `CallOption` (check `call_token_recipient`).

## Out of scope
- Trader (Buy) write flow — gated by SO‑A3.
- Scrubbing the remaining mock balances/spot/premium math — SO‑A2.

## Dependencies
- SO‑C1 (faucet) is the easiest way to get testable underlying balances, but not
  a hard blocker if the dev already holds TBTC.

---

# SO‑A2 — Replace remaining mock data in the Earn Composer with live sources

- **Epic:** EPIC A — Earn/Buy Composer — Go-Live
- **Type:** Story
- **Priority:** High (P1)
- **Estimate:** 3 pts

## Summary
`mocks/composer.ts` still fabricates wallet balances, spot price, premium math,
and the bucket cursor/queue. Replace each with the live source already used
elsewhere in the app so the Earn screen shows real numbers end-to-end.

## Current mock inventory (all in `frontend/src/mocks/composer.ts`)
1. **Spot price** — `useState(79083.44)` + a `setInterval` random walk.
2. **Wallet balances** — `const btcBalance = 0.4321; const usdcBalance = 5000.0;`
3. **Premium math** — `MOCK_PREMIUM_STRIKES` + `mockPremiumPerUnit()`; `strikes[]`
   premiums are synthetic, anchored to tile position, *not* the live RFQ.
4. **Bucket cursor/queue** — `useState<Bucket>({ cursor: 0.84, queued: 0.42, cap: 3.0 })`
   (drives `<Tideline>` and the confirm range).

> Note: strikes and RFQ quotes are **already live** — don't touch those.

## Implementation guide

### Spot price → Pyth
Use the existing `usePythPrice` hook (`frontend/src/api/usePythPrice.ts`), keyed
by the selected asset symbol, exactly as `state/dashboard.ts` does:
```ts
const live = usePythPrice(selectedAsset); // e.g. "TBTC"
const spot = live?.price ?? 0;
```
- Remove the `useState`/`setInterval` random-walk block.
- Handle `null` (feed not yet delivered): show a small "spot unavailable" note like
  the Dashboard's `dash-alert`, and keep spot-derived UI (TraderPanels) from
  dividing by zero.

### Wallet balances → on-chain coin balances
Add a small hook (e.g. `frontend/src/api/useCoinBalance.ts`) using
`useSuiClient().getBalance({ owner, coinType })` wrapped in `useQuery`
(mirror the `enabled: wallet && !!PACKAGE_ID` + `refetchInterval: 5000` pattern
from `useOwnedCallOptions.ts`). Resolve `coinType` from `series.asset_coin_type`
(underlying, for the writer balance) and `series.settlement_coin_type`.
- Replace `btcBalance`/`usdcBalance` constants with the hook results scaled by
  `series.asset_decimals` / `series.settlement_decimals`.
- `insufficientBtc` / `insufficientUsdc` then compare against real balances.

### Premiums → live RFQ only
- The displayed premium should come from the live quote, not the mock grid.
  `bestPremium` already does (`quotes[0]?.premium ?? selected.premium`). Remove
  the `MOCK_PREMIUM_STRIKES` / `mockPremiumPerUnit` fallback and stop populating
  `Strike.premium`/`premiumDisplay` from mock math. If a per-tile premium preview
  is still desired before an amount is typed, show "—" / "quote on select" rather
  than synthetic numbers.
- Audit `StrikeTiles.tsx`, `WriterPanels`, `ConfirmModal` for any reliance on the
  synthetic `premiumDisplay`; switch them to the live `bestPremium`.

### Bucket cursor / queue → `/buckets`
The live cursor + total_written are already in the `/buckets` response
(`Series.buckets[].exercise_cursor`, `total_written`, `fill_pct`,
`*_raw`). For the selected bucket, derive the `Bucket` UI shape
(`{ cursor, queued, cap }`) from those fields the same way `state/dashboard.ts`
builds `cursor` for written rows (`lookupBucketCursor` + `scaleU128`). Remove the
hardcoded `useState<Bucket>(...)`.
- `cap` ← `total_written` (or series cap if one exists); `cursor` ←
  `exercise_cursor`; `queued` ← the amount the user is about to write (their
  typed `amount`), which is what `<Tideline bucket amount>` already expects.

## Acceptance criteria
- [ ] No `MOCK_PREMIUM_STRIKES`, `mockPremiumPerUnit`, hardcoded balances, hardcoded
      spot, or hardcoded `Bucket` remain in `mocks/composer.ts`.
- [ ] Spot, balances, premium, and the Tideline cursor reflect real testnet state.
- [ ] Insufficient-balance gating uses real balances.
- [ ] Graceful empty/`null` states when a Pyth feed or balance hasn't loaded
      (no NaN, no crash).
- [ ] `npm run typecheck` passes.

## Suggested follow-up
Once nothing in the file is mocked, rename `mocks/composer.ts` →
`state/composer.ts` to match `state/dashboard.ts` (and update the import in
`Composer.tsx`). Optional but keeps the "mocks/" dir meaning "still fake".

## Dependencies
- Independent of SO‑A1, but doing A1 first is natural; they touch the same file.

---

# SO‑A3 — Gate the Buy (trader) screen behind a "Coming Soon" overlay

- **Epic:** EPIC A — Earn/Buy Composer — Go-Live
- **Type:** Story
- **Priority:** High (P1)
- **Estimate:** 1 pt

## Summary
The trader/Buy flow isn't wired to write a contract yet. Cover the **/buy**
screen with a non-dismissable "Coming Soon" overlay so it's visibly not-live,
while keeping Earn fully interactive.

## Current state
`App.tsx` routes `/buy` to `<Composer key="trader" initialView="trader" />`. The
same Composer renders for both; only `initialView` differs. The Buy CTA already
runs the same (currently mock) `submit()`.

## Implementation guide
Pick the lightest approach that matches existing styling (`styles/aqua.css`,
`styles/global.css`). Two options:

**Option 1 (recommended) — overlay inside Composer when `view === "trader"`.**
In `Composer.tsx`, when `s.view === "trader"`, render a `coming-soon` overlay
`<div>` absolutely positioned over `.app__wrap` (the WaveHero/Header stay
visible so nav still works), and set `pointer-events: none` on the underlying
composer / disable the CTA. Add a `.coming-soon` block to `global.css`
(frosted backdrop + centered "Buying calls — coming soon" copy).

```tsx
{s.view === "trader" && (
  <div className="coming-soon" role="status">
    <div className="coming-soon__card">
      <div className="coming-soon__eyebrow">trader flow</div>
      <h2>Buying calls — coming soon</h2>
      <p>Writing is live on the Earn screen. Buying is up next.</p>
    </div>
  </div>
)}
```

**Option 2 — dedicated screen.** Add `screens/ComingSoon.tsx` and point the
`/buy` route at it in `App.tsx`. Cleaner separation, but loses the "preview the
real UI behind a scrim" effect. Choose based on whether product wants the Buy UI
visible-but-disabled or fully hidden. *(Open question for product — see below.)*

Either way, keep the **Buy** nav button in `Header.tsx` (don't remove it) so the
"Coming Soon" is discoverable.

## Acceptance criteria
- [ ] Navigating to **/buy** shows a clear "Coming Soon" treatment.
- [ ] The Buy CTA cannot submit a transaction.
- [ ] **/earn** is unaffected and fully interactive.
- [ ] Header nav still lets you reach Earn/Dashboard/Activity from /buy.
- [ ] `npm run typecheck` passes.

## Open question (product)
Show the Buy UI behind a scrim (Option 1) or replace it entirely (Option 2)?
Defaulting to Option 1 unless told otherwise.

---

# SO‑B1 — Dashboard: remove residual hardcoded values & add connection gating

- **Epic:** EPIC B — Account Surfaces — Dashboard & Activity
- **Type:** Story
- **Priority:** Medium (P2)
- **Estimate:** 2 pts

## Summary
The Dashboard is already live (`state/dashboard.ts` pulls positions, owned
options, buckets, and Pyth spot, and `submit()` builds real exercise/redeem
PTBs). A few mock/hardcoded cosmetics remain and there's no "connect your
wallet" state. Finish the scrub so the Dashboard is launch-clean.

## Current state — hardcoded bits in `frontend/src/screens/Dashboard.tsx`
1. Hero address is literal: `connected · 0x9f3a…42b1` (`dash-hero__addr`).
2. "Avg fill latency `142 ms`" cell in the **written** summary is fabricated.
3. No disconnected state — `useDashboardState` exposes `connected`, but
   `Dashboard.tsx` never uses it; a logged-out user sees empty "no calls" CTAs
   rather than a "connect wallet" prompt.

## Implementation guide
- **Address:** replace the literal with the real connected address. `useDashboardState`
  already has `account`; expose `address` from the hook (like `composer`/`dashboard`
  do elsewhere) and render `shortAccount(address)`; show nothing / "not connected"
  when `!connected`.
- **Avg fill latency:** either remove the cell, or back it with a real metric. The
  RFQ/quoting layer doesn't currently expose per-fill latency
  (`rfqEntriesToUi` sets `latency: 0` with a comment that the service doesn't
  surface it). Recommend **removing the cell** for launch and filing a follow-up if
  product wants latency. Don't ship a fabricated 142 ms.
- **Connection gating:** when `!d.connected`, render a "connect your wallet to see
  your positions" panel (reuse `dash-empty` styling) instead of the owned/written
  empty CTAs. The Header already owns the connect modal.

## Acceptance criteria
- [ ] Hero shows the actual connected address (or a clean disconnected state).
- [ ] No fabricated latency number ships (cell removed or real).
- [ ] Disconnected users get a "connect wallet" prompt, not misleading empty
      states.
- [ ] No other literals (`0x9f3a…42b1`, `142`) remain in `Dashboard.tsx`.
- [ ] `npm run typecheck` passes.

## Out of scope
- The exercise/redeem PTBs (already live and correct).

---

# SO‑B2 — Activity: replace mock seed with indexer-backed event log + WSS tail

- **Epic:** EPIC B — Account Surfaces — Dashboard & Activity
- **Type:** Story
- **Priority:** Medium (P2)
- **Estimate:** 5 pts

## Summary
The Activity screen UI is built but runs entirely on a hardcoded seed
(`ACTIVITY_SEED` in `mocks/activity.ts`). Build a live data source backed by the
api-service event log, with a live tail over the quoting-service WSS, returning
the same `ActivityState` shape so `Activity.tsx` stays unchanged.

## Current state
- `frontend/src/screens/Activity.tsx` consumes `useActivityState()` from
  `mocks/activity.ts` and a hardcoded address string `0x9f3a…42b1` in the hero.
- `mocks/activity.ts` exports `ACTIVITY_SEED: ActivityEvent[]`, the
  `EVENT_TYPE_META` map, filter list, `formatDay`/`relativeTime` helpers, and
  `useActivityState()` (filtering + day-grouping + a `now` ticker + totals).
- Domain types already exist: `ActivityEvent`, `ActivityTotals`, `GroupedEvent`,
  `EventStatus` (`frontend/src/types.ts`).

## Backend dependency (verify first)
There is **no `/events` (or activity) endpoint** in `frontend/src/api/client.ts`
today — it only has `/buckets`, `/positions`, `/call-token-lots`. Before UI work:
- Confirm whether `rust-backend/services/api-service` already emits a per-wallet
  event feed (check its `README.md` / `src/handlers/`). The on-chain events exist
  (`contracts/sources/events.move`: `emit_write_executed`, `emit_bucket_created`,
  exercise/claim events, etc.) and the indexer consumes them, so the data is
  present — the question is whether an HTTP endpoint is exposed.
- The hero copy and `dash-hero__eyebrow` ("on-chain log · indexer-backed",
  "live tail via wss") imply a WSS tail is intended; the quoting WSS client lives
  at `frontend/src/api/quotingClient.ts` and already handles `BucketUpdate`
  pushes (`ServiceToRetail` in `quoting.ts`).

If the endpoint doesn't exist yet, **split**: a backend sub-task to add
`GET /events?wallet=…` (+ optional WSS event stream), and this frontend ticket
depends on it.

## Implementation guide

### 1. Client types + fetch — `frontend/src/api/client.ts`
Add an `EventDto` mirroring the api-service handler and a `fetchActivity(wallet)`
following the exact shape of `fetchPositions`/`fetchCallTokenLots` (same error
handling: `GET /events failed: <status>`). Map `EventDto` → the existing
`ActivityEvent` type (id, ts ISO, type, side, status, title, body, optional
`value {delta, unit}`, `txHash`, `bucket`). Keep `*_raw` → scaled conversion
consistent with the dashboard (`scaleU64`/`scaleU128`, divide by decimals).

### 2. Live state hook — `frontend/src/state/activity.ts` (new)
Port the pure logic from `mocks/activity.ts` (filtering, day-grouping, totals,
`now` ticker, `formatDay`/`relativeTime`, `EVENT_TYPE_META`, `ACTIVITY_FILTERS`)
into a real hook:
- `usePositions`-style `useQuery(["events", wallet], () => fetchActivity(wallet), { enabled, refetchInterval })`.
- Compute `totals` from the live events (the seed currently hardcodes some — derive
  exercises/writes/buys/deposits/premiumIn/premiumOut by reducing over events).
- Live tail: subscribe to the quoting WSS (or a new events WSS) and prepend new
  events, deduping by `id`. Reuse `quotingClient` if it can carry event frames;
  otherwise a dedicated `EventSource`/WS is fine.
- Keep the **exact** return shape `useActivityState()` exposes today
  (`events`, `grouped`, `filter`, `setFilter`, `totals`, `now`) so `Activity.tsx`
  doesn't change.

### 3. Wire the screen — `frontend/src/screens/Activity.tsx`
- Switch the import from `../mocks/activity` to `../state/activity` (re-export the
  helpers/meta/filters from the new module so the named imports still resolve).
- Replace the hardcoded `0x9f3a…42b1` hero address with the connected wallet
  (or a "connect wallet to see your activity" empty state when disconnected).

## Acceptance criteria
- [ ] Activity renders real on-chain events for the connected wallet (writes,
      buys, exercises, claims, deposits, cursor advances) with correct
      values/units and tx links.
- [ ] Filters (All/Trades/Writes/Exercise/Claims/Account) operate on live data.
- [ ] New events appear without a manual refresh (poll and/or WSS tail).
- [ ] Totals are derived from live events, not hardcoded.
- [ ] Disconnected state is handled (no fake address).
- [ ] `npm run typecheck` passes.

## Dependencies
- **Blocked by** api-service exposing an events endpoint (and ideally a WSS event
  stream). Confirm/raise the backend sub-task before starting frontend work.

---

# SO‑C1 — Testnet faucet page for minting test tokens

- **Epic:** EPIC C — Testnet Developer Tooling
- **Type:** Story
- **Priority:** Medium (P2)
- **Estimate:** 2 pts

## Summary
Add a dedicated testnet-only page that lets a connected wallet mint the protocol
test tokens (TBTC, TUSDC, TDEEP, TWAL) so devs/testers can fund themselves to
exercise the Earn flow.

## On-chain target
Each test token is a shared `Faucet` with a public mint
(`test-tokens/sources/{tbtc,tusdc,tdeep,twal}.move`):
```move
public fun mint(faucet: &mut Faucet, amount: u64, ctx): Coin<T>
public fun mint_to_sender(faucet: &mut Faucet, amount: u64, ctx)  // simplest
```
`mint_to_sender` mints and transfers to `ctx.sender()` — ideal for a faucet
button (no manual transfer step).

### IDs (already in `rust-backend/deployments.json` → `testnet.package_info.testTokens`)
| Token | Module | Faucet id field | Decimals |
|-------|--------|-----------------|----------|
| TBTC  | `…::tbtc::TBTC`   | `tokens.TBTC.faucetId`  | 8 |
| TUSDC | `…::tusdc::TUSDC` | `tokens.TUSDC.faucetId` | 6 |
| TDEEP | `…::tdeep::TDEEP` | `tokens.TDEEP.faucetId` | 6 |
| TWAL  | `…::twal::TWAL`   | `tokens.TWAL.faucetId`  | 9 |

Test-tokens package id: `testnet.package_info.testTokens.packageId`.

## Implementation guide

### 1. Expose test-token config — `frontend/src/config.ts`
`config.ts` already reads `deployments.json` for `PACKAGE_ID` etc. Add a typed
export for the test tokens, e.g.:
```ts
const tt = info?.testTokens;
export const TEST_TOKENS = tt
  ? Object.entries(tt.tokens).map(([symbol, t]) => ({
      symbol, coinType: t.coinType, faucetId: t.faucetId, decimals: t.decimals,
      packageId: tt.packageId,
    }))
  : [];
```
Guard for the `null` (mainnet/devnet) case so non-testnet envs yield `[]` — the
page then shows a "testnet only" empty state instead of crashing (same philosophy
as the existing `PACKAGE_ID: string | undefined`).

### 2. PTB builder — `frontend/src/tx/faucet.ts` (new)
```ts
export function buildMintTx(p: {
  testTokenPackageId: string; module: string; // e.g. "tbtc"
  faucetId: string; amountRaw: bigint;
}): Transaction {
  const tx = new Transaction();
  tx.moveCall({
    target: `${p.testTokenPackageId}::${p.module}::mint_to_sender`,
    arguments: [tx.object(p.faucetId), tx.pure.u64(p.amountRaw)],
  });
  return tx;
}
```
(Derive `module` from the coinType's middle segment, or store it in `TEST_TOKENS`.)

### 3. Screen — `frontend/src/screens/Faucet.tsx` (new)
- Mirror `Admin.tsx` structure: `WaveHero` + `Header`, `dash-hero`, a `run(key,
  build, ok)` sign+execute helper (copy the idiom from `Admin.tsx:97` — busy key,
  `signAndExecute`, `flash("✓ …")`, `failed · <message>`).
- One row/card per token in `TEST_TOKENS`: symbol, an amount input (default a
  sensible amount, e.g. 1 TBTC = `100_000_000` raw), and a "Mint" button that
  builds `amountRaw = round(amount * 10^decimals)` and runs `buildMintTx`.
- Empty state when `TEST_TOKENS.length === 0` ("Faucet is testnet-only; current
  env is `{ENV}`").
- Optional: show the wallet's current balance per token (reuse the
  `getBalance` hook from SO‑A2 if it lands).

### 4. Route + nav
- Add `<Route path="/faucet" element={<Faucet />} />` in `App.tsx`.
- Add a **Faucet** nav button in `Header.tsx`. Recommend gating it on
  `ENV === "testnet"` so it never shows on a mainnet build (parallel to how the
  Admin button is gated on `isAdmin`).

## Acceptance criteria
- [ ] On testnet, a connected wallet can mint each of TBTC/TUSDC/TDEEP/TWAL and the
      coins land in their wallet (verify via balance / explorer).
- [ ] Amounts respect each token's decimals (1.0 TBTC → `100000000` raw).
- [ ] On non-testnet envs the page/nav entry is hidden or shows a "testnet-only"
      state — no crash from `null` deployment blocks.
- [ ] Tx failures surface the chain error via toast.
- [ ] `npm run typecheck` passes.

## Verification
Mint TBTC here, then complete SO‑A1's write flow end-to-end as the same wallet.

---

# SO‑D1 — Protocol/contracts: the one remaining change (needs detail)

- **Epic:** EPIC D — Protocol / Contracts
- **Type:** Task
- **Priority:** TBD
- **Estimate:** TBD

## Summary
The planning thread mentions "I have 1 thing I need to do to the contracts" with
no specifics. This is a placeholder so the work isn't lost — **it needs the
author to specify the exact change** before it can be turned into an
implementation guide.

## Needed before this can be scoped
- Which module/behavior changes (`bucket`, `quote`, `account`, `treasury`, …)?
- Is it a bug fix, a new entrypoint, or a signature change?
- Does it affect the frontend PTB builders (`tx/admin.ts`, `tx/dashboard.ts`, and
  the new `tx/composer.ts` from SO‑A1)? A signature change to `execute_write`,
  `exercise`, or `redeem_position` would ripple into those.
- Does it require a redeploy (new `deployments.json` ids) and a re-index?

## Acceptance criteria
- [ ] Author fills in the concrete change; ticket is rewritten as a real guide.

---

## Cross-cutting notes
- All new transaction code should reuse the established idioms: `requirePackage()`
  guard, `useSignAndExecuteTransaction`, the `signing → broadcast → confirmed`
  stages, and `failed · <message>` toasts (see `screens/Admin.tsx` and
  `state/dashboard.ts`).
- Keep the "live drop-in replacement, same return shape" pattern that
  `state/dashboard.ts` established so screen components don't change when a mock
  becomes live.
- Run `npm run typecheck` (and `npm run build`) before each PR.
