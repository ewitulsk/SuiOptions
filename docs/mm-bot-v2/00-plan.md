# mm-bot V2 reset: delta-hedged vol desk (V1 long-only, V2 two-sided)

Status: **PLANNED, not scheduled**. This documents the agreed strategy reset
for the mm-bot: kill every existing trading strategy and rebuild as a
delta-hedged volatility desk in two stages. Nothing here is implemented;
prerequisite integrations (perps venue, DeepBook flash loans, curator
dashboard) are being explored first — see `01-perps-venues.md`.

## The strategies (agreed spec)

### V1 — delta-hedged long-vol fund

One-liner: pays retail for their covered calls, earns spread + gamma
scalping + funding, uses atomic flash-exercise to make monetization
capital-light. Its enemy isn't a crash — it's a boring market plus
overpaying. The two things to build most carefully: the **theta governor**
and the **bid-pricing discipline**.

1. **Pricing engine** — vol surface anchored to realized vol (EWMA over
   1d/7d/30d), risk premium on top, skew/term shaped from BTC/ETH surfaces
   as priors scaled by vol ratio. American calls priced binomial/BAW with
   staking yield as the dividend rate (drives early-exercise optimality).
   Bid = model fair − base spread − inventory penalty − size penalty −
   hedge-cost estimate (expected funding over holding period + slippage).
   Quote every RFQ; degrade with size/inventory, never refuse. Flow is
   one-directional and mildly adversely selected (retail overwrites after
   run-ups) — base spread wider than a two-sided maker's.
2. **The book** — long calls delta-hedged with perps = long-vol book.
   Tails are profitable; the failure mode is slow bleed (sideways market:
   theta eats premium, scalping doesn't recoup). Everything is oriented
   around making the bleed survivable and the convexity monetizable.
3. **Delta hedging** — net book delta continuously, short perps against
   it, rebalance on **bands not clocks** (±X% NAV; wider when funding is
   expensive). Long-gamma rebalancing = systematically sell high/buy low —
   the primary revenue engine. Track explicitly: if (scalping + spread) <
   (theta + funding) over a rolling window, bids are too high or bands
   wrong. Live funding feeds both bid pricing and band width.
4. **Vega/inventory limits** — vega only accumulates (no venue to sell
   it). Cap: max net vega %NAV, max theta burn/day %NAV (the bleed
   governor), premium concentration per expiry/strike bucket, max %NAV in
   premium overall. At limits, bids widen sharply; never stop quoting.
5. **Exit & monetization** (ranked): (1) resell via secondary/RFQ when a
   bid ≥ model − small concession (taker only); (2) hold to expiry, gamma
   scalp — the default; (3) exercise when optimal — near expiry ITM, or
   early when forgone staking yield > remaining time value (automated,
   never manual). **Exercise mechanics**: vault cash first; **flash-loan
   fallback** (borrow strike → exercise → sell underlying on spot → repay
   → pocket the difference) so ITM exercises are never
   capital-constrained; pre-simulate incl. own-book slippage, ladder big
   size, abort if net ≤ 0. Unwind the corresponding perp short in the same
   breath — the exercise sale and hedge unwind are the same trade.
6. **Capital structure** — three buckets: premium budget (capped %NAV,
   live quotes reserve premium until fill/TTL), hedge margin buffer
   (stress: shorts get margin-pressed in rallies exactly when calls
   explode — marks and margin calls aren't simultaneous; buffer for a
   +50% gap; flash-exercise is the pressure valve), liquidity reserve (LP
   withdrawals + preferred exercise funding — smaller thanks to flash
   loans). Reservations + deployed ≤ NAV, always.
7. **Marks/NAV/ops** — strictly mark-to-model; secondary prints are
   sanity checks. Automated exercise keeper **with redundancy** (missing
   an ITM expiry is a pure LP loss). Monitors: delta vs band, vega vs
   cap, theta vs governor, funding, scalp P&L vs bleed, reserves, margin
   headroom. Stress: −60% gap, +80% gap, 6-month flat, funding −50%/30d.

#### V1 starting parameters (revisit after 60–90 days of flow)

- Quote TTL 30s (10–15s when 1-min RV > 2× 24h avg)
- Base spread: bid at fair − 4–6 vol pts; size penalty +1 vol pt per
  (notional / 1% NAV), ~quadratic beyond 3% NAV; max single fill 5% NAV
  premium; inventory penalty → −10 vol pts as vega utilization 60→100%
- Premium budget 30% NAV (hard 35%); vega cap: +10 vol pts may move NAV
  ≤ +5% (net vega ≤ 0.5% NAV/vol pt); theta governor ≤ 15bps NAV/day
  (soft-throttle from 10); concentration ≤ 30%/expiry, ≤ 15%/strike
  bucket (<90 / 90–110 / >110% moneyness)
- Delta band ±1.5% NAV (2.5% when shorting funding > ~25% ann.); hedge
  margin 3× initial, sized for +75% gap; liquidity reserve 10% NAV
  (floor 5%)
- Early-exercise check daily: exercise when forgone yield > remaining
  time value × 1.1; kill switch: stop new buys if NAV −10% in 7d

### V2 — the two-sided maker

Adds writing covered calls + cash-secured puts. Identity change: a true
market maker whose core asset is **netting** between retail sellers and
buyers. Short-vol capacity is scarce and priced — sell vol freely up to
what was bought from retail plus a small hard-capped naked budget.

1. **Signed vega band** −0.15% to +0.5% NAV/vol pt (asymmetric: long vol
   bleeds slowly, short vol gaps fatally; only long has a natural exit).
2. **Two-sided quoting with inventory skew** — effective mid = fair +
   k × (net vega / band width); ±3 vol pt base spread (netting allows
   tighter than V1); asymmetric size caps (write 3% / buy 5% NAV, write
   size → 0 at the short edge); near-expiry ATM throttle (2× spread, ½
   size inside 48h).
3. **Netting engine** — net Greeks per expiry bucket, hedge residuals
   only. Protocol features to build (the binding constraint is
   collateral): **spread collateral compression** (long call offsets
   written call at equal-or-higher strike, same expiry) and
   **exact-offset closure** (net same-series long/short to zero, free
   both collaterals). Staked-SUI as call collateral is a later yield
   enhancement.
4. **Short-side mechanics** — covered calls via **JIT spot acquisition**
   (atomically buy underlying in the mint tx; never warehouse
   inventory); CSPs lock strike USDC, hedge the long delta. **Assignment
   forecast**: run the early-exercise model in reverse over written
   positions, pre-unwind hedges when assignment is rational; the
   assignment-detection keeper adjusting perps within a block is
   critical infrastructure.
5. **Gamma/pin risk** — net gamma cap (10% spot move → ≤3% NAV delta
   change per expiry); buy back short ATM series in the final 24–48h
   even at ~1 vol pt concession.
6. **Capital** — premium budget 20% NAV; collateral pool ≤ 40% NAV;
   hedge margin 3× stressed both directions (worse of ±60% gap);
   liquidity reserve 10% NAV; per-quote reservation covers collateral /
   JIT budget + hedge margin.
7. **Limits** — naked short vega ≤ 0.1% NAV/vol pt; theta between
   −15bps and +10bps NAV/day (cap the income: theta collected is gamma
   risk worn); written notional ≤ 60% NAV (≤ 25%/expiry); daily stress
   gates (−60%, +80%, ±40% w/ vol +15, flat 6mo, funding −50%/30d) must
   show < 25% drawdown or new risk is blocked.
8. **Earnings stack** — two-way spread (much of it internally netted ≈
   riskless), gamma scalp on the long residual, bounded theta on the
   short residual, funding + staking yield on hedge/collateral. The
   vault is a router with a warehouse; the limits keep the warehouse
   small.

## Implementation plan

### Phase 0 — Teardown

Kill the strategy layer: WS-RFQ quote pricing (vol markup/smile model),
`onchain_rfq`/`onchain_put_rfq` bidders, `onchain_swap`, the DeepBook
option-pool quoter, the `[trading_vault]` vault-mode quoter, and their
config. Keep the chassis: token-info client, oracle WS feed, quoting WS
transport, tx submission, observability, `mm_collateral` release,
`RollingVolBuffer`, the testnet `[sim]` harness (becomes the test
counterparty), BalanceManager plumbing.

**Architecture decision (recommended): strategies run as the curator of a
TradingVault.** NAV, LP flows, appraisal, custody, and keeper cranks
already exist there; V1's book (held calls) is exactly the custody state
made appraisable by SO-297. The bot holds the CuratorCap; the vault holds
the money.

### Phase 1 — Pricing engine (pure Rust)

`pricing::surface` (multi-window EWMA + risk premium + configurable
skew/term prior; external BTC/ETH anchor behind a trait, fast-follow) and
`pricing::american` (CRR binomial as exercise-boundary oracle, BAW for
hot-path quoting, continuous dividend = staking yield). Bid formula as
separately unit-tested terms. All V1 parameters as a `[v1]` config block.

### Phase 2 — Book & risk state

Single-source-of-truth `book` module: inventory reconstructed from vault
custody on boot; net Greeks per expiry; limits engine exposing continuous
utilization (feeds inventory penalty — widen, never stop); reservation
ledger (`reservations + deployed ≤ NAV` before every quote); persisted
P&L attribution lines (spread / scalp / theta / funding) powering the
bleed alarm.

### Phase 3 — V1 execution

RFQ bidder across both channels (WS RFQs + on-chain auctions); delta
hedger behind a `HedgeVenue` trait (`paper` for testnet — simulated fills,
real accounting; real venue per `01-perps-venues.md`; `spot-borrow`
fallback); exit ladder (taker-only resale into option-coin DeepBook pools
≥ model − concession → hold → automated exercise); **flash-exercise** via
DeepBook v3 flash loans (borrow quote → exercise → sell underlying →
repay, dev-inspect pre-simulation, laddering, abort if net ≤ 0);
redundant ITM-expiry sweep in the keeper service.

### Phase 4 — V1 ops

Monitors with `alert_id`s (delta/vega/theta/scalp-vs-bleed/reserves/
margin headroom/kill switch) and the stress suite as a nightly job
against live inventory.

### Phase 5 — V2 protocol prerequisites (contracts, parallel track)

Spread collateral compression, exact-offset closure, (later) staked-SUI
collateral. V2 quoting can ship fully-collateralized before these land;
they gate capital efficiency, not function.

### Phase 6 — V2 desk

Skewed mid + signed vega band + asymmetric caps; naked-short budget
tracking; JIT spot in the mint tx; assignment forecaster + hedge
pre-unwind; pin-risk throttle + ATM buyback; V2 capital buckets + daily
stress gates.

### Sequencing

| Stage | Contents | Ships |
|---|---|---|
| 0 | Teardown + vault-custody wiring | staging immediately |
| 1–2 | Pricing + book/risk (bulk of careful work) | parallel |
| 3–4 | V1 loops, `paper` hedge venue, vs the sim | V1 on staging |
| 5 | Contract features (compression/netting) | parallel track |
| 6 | V2 desk, still `paper` hedged | V2 on staging |
| — | Real perps venue + mainnet params | gates real money |

### Open decisions

1. Vault-custody architecture (recommended) vs bot-own-wallet — pending.
2. Perps venue — under exploration (`01-perps-venues.md`).
3. Sim harness stays as test counterparty; option-coin DeepBook pools
   stay as V2's secondary/resale channel — recommended, pending.
4. Epic structure: one epic ~12 tickets vs split V1/V2 epics — pending.
