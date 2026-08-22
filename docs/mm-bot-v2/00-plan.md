# mm-bot reset: delta-hedged long-option desk

Status: **PLANNED, not scheduled**. This documents the agreed strategy reset
for the mm-bot: kill every existing trading strategy and rebuild as a
delta-hedged buyer of options written through the Earn page. The reset is
not implemented end to end; prerequisite integrations (perps venue,
DeepBook flash loans, curator dashboard) are being explored first — see
`01-perps-venues.md`.

## The strategy (agreed spec)

### Delta-hedged long-vol fund

One-liner: pays retail for covered calls and cash-secured puts written
through the Earn page, earns spread + gamma scalping, pays or receives
funding according to hedge direction, and uses automated exercise to
monetize long options. The desk is always the option
buyer: "covered" and "cash-secured" describe the retail writer's collateral,
not positions written by the desk. Its enemy isn't a large move — it's a
boring market plus overpaying. The two things to build most carefully are the
**theta governor** and the **bid-pricing discipline**.

1. **Pricing engine** — vol surface anchored to realized vol (EWMA over
   1d/7d/30d), risk premium on top, skew/term shaped from BTC/ETH surfaces
   as priors scaled by vol ratio. American calls and puts priced binomial/BAW
   with staking yield as the dividend rate (drives exercise optimality).
   Bid = model fair − base spread − inventory penalty − size penalty −
   hedge-cost estimate (expected funding over holding period + slippage).
   Quote every eligible RFQ and degrade with size/inventory. Decline on
   stale data, hard size or solvency limits, an active kill switch, or an
   unsafe execution path. Flow is
   one-directional in role — retail writes, the desk buys — and mildly
   adversely selected. Covered-call flow increases after run-ups; put-writing
   flow may increase after sell-offs. Base spread is wider than a two-sided
   maker's.
2. **The book** — long calls and long puts, delta-hedged with perps, form a
   long-vol book. Large moves in either direction create convex option gains,
   but realized profit still depends on entry price, hedge path, funding,
   margin, and monetization costs. The recurring failure mode is slow bleed
   (sideways market: theta eats premium, scalping doesn't recoup). Everything
   is oriented around making the bleed survivable and the convexity
   monetizable.
3. **Delta hedging** — net book delta continuously and take the opposite,
   signed perp position: short perps against positive call delta and long
   perps against negative put delta. Rebalance on **bands not clocks** (±X%
   NAV; widened when execution or funding makes the required hedge direction
   expensive). Long-gamma rebalancing systematically sells high and buys low
   — the primary revenue engine. Track exact realized option, hedge, funding,
   and execution P&L separately from model edge. If realized gains do not
   cover theta and execution over a rolling window, bids are too high or
   bands are wrong. Live direction-aware funding cost feeds both bid pricing
   and band width.
4. **Vega/inventory limits** — vega only accumulates (no venue to sell
   it). Cap: max net vega %NAV, max theta burn/day %NAV (the bleed
   governor), premium concentration per expiry/strike bucket, max %NAV in
   premium overall. At soft limits, bids widen sharply; hard solvency,
   concentration, and execution limits decline the RFQ.
5. **Exit & monetization** (ranked): (1) resell via secondary/RFQ when a
   bid ≥ model − small concession (taker only); (2) hold to expiry, gamma
   scalp — the default; (3) exercise ITM options when optimal, always before
   expiry. **Call exercise:** vault cash first; flash-loan fallback borrows
   strike, exercises, sells the received underlying, repays, and pockets the
   difference. **Put exercise:** choose the first profitable, available path
   in this atomic PTB waterfall: (a) use vault underlying, exercise, then use
   part of the strike payout to repurchase the delivered underlying; (b)
   flash-borrow underlying, exercise, repurchase the exact repayment amount
   from the strike payout, then repay; (c) flash-borrow settlement, swap it to
   the required underlying, exercise, then repay settlement from the strike
   payout. Every path restores or repays its underlying source and leaves the
   residual profit in settlement. Pre-simulate including slippage, flash
   capacity, exact repayment, and gas; require a configured minimum net
   profit and abort the whole PTB otherwise. Unwind the corresponding signed
   perp hedge as tightly as the venue permits; Bluefin execution is not
   atomic with the on-chain exercise.
6. **Capital structure** — three buckets: premium budget (capped %NAV,
   live quotes reserve premium until fill/TTL), hedge margin buffer
   (short hedges get margin-pressed in rallies when calls explode; long
   hedges get pressed in crashes when puts explode; marks and margin calls
   aren't simultaneous; buffer for gaps in both directions), liquidity reserve (LP
   withdrawals + preferred exercise funding — smaller thanks to flash
   loans). Reservations + deployed ≤ NAV, always.
7. **Marks/NAV/ops** — strictly mark-to-model; secondary prints are
   sanity checks. Automated exercise keeper **with redundancy** (missing
   an ITM expiry is a pure LP loss). Monitors: delta vs band, vega vs
   cap, theta vs governor, funding, scalp P&L vs bleed, reserves, margin
   headroom. Stress: −60% gap, +80% gap, 6-month flat, funding −50%/30d.

#### Starting parameters (revisit after 60–90 days of flow)

- Quote TTL 30s (10–15s when 1-min RV > 2× 24h avg)
- Base spread: bid at fair − 4–6 vol pts; size penalty +1 vol pt per
  (notional / 1% NAV), ~quadratic beyond 3% NAV; max single fill 5% NAV
  premium; inventory penalty → −10 vol pts as vega utilization 60→100%
- Vault-scaled launch policy uses conservative `risk_nav` from doc 08:
  premium budget soft-throttles at 25% and hard-caps at 30%; calls and puts
  each hard-cap at `min(20% × risk_nav, effective type capacity)`; combined
  premium per expiry hard-caps at `min(10% × risk_nav, effective expiry
  capacity)`. Vega cap: +10 vol pts may move NAV ≤ +5% (net vega ≤ 0.5%
  NAV/vol pt); theta governor ≤ 15bps NAV/day (soft-throttle from 10);
  strike-bucket concentration ≤ 10% NAV (<90 / 90–110 / >110% moneyness)
- Provisional call-book delta band ±15% NAV (25% when execution or funding
  for the required hedge direction is expensive), per doc 07. Put-only and
  mixed-book bands are not inherited from the call-only result; the
  backtester must select them out of sample. Hedge margin 3× initial, sized
  for the worse of a +75% or −60% gap and the configured call/put mix;
  liquidity reserve 10% NAV (floor 5%)
- Exercise check daily plus a redundant near-expiry sweep for both calls and
  puts. Put exercise requires net profit after all modeled costs of at least
  `max($10 settlement equivalent, 5bps × strike payout, 2 × route
  uncertainty)`. Kill switch: stop new buys if NAV −10% in 7d
- Product hurdle: net annualized return ≥ `max(12%, settlement cash yield +
  8%)`; historical drawdown ≤ 15%; required-stress drawdown ≤ 25%; zero
  liquidations; required 24h margin top-up ≤ the lesser of 10% risk NAV and
  remaining external-account release capacity

## Implementation plan

### Phase 0 — Teardown

Kill the strategy layer: WS-RFQ quote pricing (vol markup/smile model),
`onchain_rfq`/`onchain_put_rfq` bidders, `onchain_swap`, the DeepBook
option-pool quoter, the `[trading_vault]` vault-mode quoter, and their
config. Keep the chassis: token-info client, oracle WS feed, quoting WS
transport, tx submission, observability, `mm_collateral` release,
`RollingVolBuffer`, the testnet `[sim]` harness (becomes the test
counterparty), BalanceManager plumbing.

**Architecture decision (recommended): the strategy runs as the curator of a
TradingVault.** NAV, LP flows, appraisal, custody, and keeper cranks
already exist there; the long call/put book is exactly the custody state
made appraisable by SO-297. The bot holds the CuratorCap; the vault holds
the money.

### Phase 1 — Pricing engine (pure Rust)

`pricing::surface` (multi-window EWMA + risk premium + configurable
skew/term prior; external BTC/ETH anchor behind a trait, fast-follow) and
`pricing::american` (call and put CRR binomial as exercise-boundary oracle,
BAW for hot-path quoting, continuous dividend = staking yield). Bid formula
as separately unit-tested terms. All strategy parameters in one config block.

### Phase 2 — Book & risk state

Single-source-of-truth `book` module: long call and put inventory
reconstructed from vault custody on boot; signed net Greeks per expiry;
limits engine exposing continuous utilization (feeds inventory penalties
and hard declines); reservation ledger (`reservations + deployed ≤ NAV`
before every quote); exact P&L ledger plus explanatory model-edge, option,
hedge, theta, funding, and execution attribution powering the bleed alarm.

### Phase 3 — execution

Earn-side RFQ bidder for both covered-call and cash-secured-put writer flow,
across WS RFQs and on-chain auctions; signed delta hedger behind a
`HedgeVenue` trait (`paper` for testnet — simulated fills, real accounting;
real venue per `01-perps-venues.md`); exit ladder (taker-only resale into
option-coin DeepBook pools ≥ model − concession → hold → automated
exercise); flash/capital-efficient call and put exercise with dev-inspect
pre-simulation, laddering, and abort-if-net-nonpositive; redundant ITM-expiry
sweep for both option types in the keeper service.

### Phase 4 — ops

Monitors with `alert_id`s (delta/vega/theta/scalp-vs-bleed/reserves/
margin headroom/kill switch) and the stress suite as a nightly job
against live inventory.

### Sequencing

| Stage | Contents | Ships |
|---|---|---|
| 0 | Teardown + vault-custody wiring | staging immediately |
| 1–2 | Pricing + book/risk (bulk of careful work) | parallel |
| 3–4 | Call + put Earn-side loops, signed `paper` hedge venue, vs the sim | long-option desk on staging |
| — | Real perps venue + mainnet params | gates real money |

### Open decisions

1. Vault-custody architecture (recommended) vs bot-own-wallet — pending.
2. Perps venue — **Bluefin Pro**, sole venue (`01-perps-venues.md`;
   DeepBook Margin removed by SO-334, see `06-dbm-removal.md`).
3. Sim harness stays as test counterparty; option-coin DeepBook pools
   stay as the secondary/resale channel — recommended, pending.
4. Put exercise waterfall is decided (§5). Provider/pool allowlists, route
   selection, flash-capacity limits, and the composed vault/adapter entry are
   implementation work and gate buying puts safely.
