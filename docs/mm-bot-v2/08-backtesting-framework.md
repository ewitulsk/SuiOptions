# Backtesting framework — final design and delivery plan

Status: **PLANNED, not started. 2026-08-22.** This is the implementation
plan for testing the single long-option strategy in `00-plan.md`. It
supersedes the earlier design in this file and supersedes
`rust-backend/crates/vault-sim` and `rust-backend/tools/backtester`, which
target the deprecated covered-call vault.

Companion docs:

- `09-backtesting-gap-remediation.md` — 2026-09-01 review follow-up:
  reorders delivery, adds the IV estimator phase, and supersedes §1.6/§6.1
  with an oracle-agnostic rule (both Pyth and Switchboard, no
  oracle-specific data).
- `00-plan.md` — the strategy contract.
- `07-backtest-data-and-cost-findings.md` — call-only measurements and data
  findings that this framework must reproduce and extend.
- `data-room/docs/sui-collection-plan.md` — source collection.
- `data-room/docs/l2-silver-schema-plan.md` — depth normalization.

---

## 0. Objective, scope, and epistemic boundary

### 0.1 The strategy being tested

The desk is always the option buyer and the Earn user is always the option
writer:

- The desk buys covered calls written by users who supply the underlying.
- The desk buys cash-secured puts written by users who supply settlement
  collateral.
- The desk never writes an option.
- Long calls normally require a short-perp hedge.
- Long puts normally require a long-perp hedge.
- Calls and puts are hedged as one signed net-delta book.

The strategy earns or loses money through realized option payoff/resale,
gamma scalping, funding, execution, and the difference between priced and
realized variance. Internal model fair value is not revenue.

### 0.2 What this framework can claim

There is no historical Earn RFQ or acceptance dataset. Market conditions
can be replayed historically, but option arrivals and customer decisions
must initially be modeled.

Results must therefore be labeled **conditional historical simulation**:

> Given a stated Earn-flow model, latency model, execution model,
> oracle-proxy model, and historical market path, how would the production
> strategy have performed?

The framework can answer:

- Whether the desk survives historical and synthetic market stress.
- Whether margin can be maintained while option gains are trapped on Sui.
- Which hedge bands break even after turnover and execution costs.
- How call/put mix changes funding, margin, exercise, and P&L.
- What bid, flow, and execution assumptions are required to clear a return
  hurdle.
- At what NAV hedge and exercise liquidity become binding.

It cannot establish an unconditional historical APY until Earn arrival and
acceptance models are calibrated from live **mainnet** outcomes (see the
decision under §3.1).

### 0.3 Explicit non-goals

- The desk never writes options; short-option assignment, collateral
  compression, pin risk, and naked-vega budgets are out of scope.
- Endogenous LP deposit/withdrawal behavior is not forecast. Vault growth is
  evaluated as a grid of starting NAVs; queued withdrawals and configurable
  withdrawal shocks still reduce available capital in capacity runs.
- Exact queue position cannot be recovered from market-by-price L2 data.
  Passive execution is reported as bounded/swept assumptions, not fact.
- Binance history is a proxy for missing Bluefin history. It cannot prove
  that a Bluefin account would have survived.

### 0.4 Vault-scaled launch policy

The framework must not choose its own success criteria after observing the
results. Store policy in ratios/basis points and derive absolute limits from
the current vault, rather than putting fixed dollar amounts in bot config.
The launch policy is:

| Decision | Launch policy |
|---|---:|
| Net annualized return hurdle | `max(12%, settlement cash yield + 8%)`, **depositor-net**: after every desk cost AND after the trading vault's curator performance fee and the protocol's share of it (doc 09 G7, 2026-09-02) |
| Maximum historical drawdown | 15% of risk NAV |
| Maximum synthetic-stress drawdown | 25% of risk NAV |
| Liquidation tolerance | Zero in historical replay and required stresses |
| Minimum put-exercise profit | `max($10 settlement equivalent, 5 bps × strike payout, 2 × route uncertainty)` after modeled costs |
| Maximum premium in calls | `min(20% × risk NAV, effective call capacity)` |
| Maximum premium in puts | `min(20% × risk NAV, effective put capacity)` |
| Maximum total option premium | 30% of risk NAV |
| Maximum premium per expiry, calls + puts | `min(10% × risk NAV, effective expiry capacity)` |
| Maximum required margin top-up in 24h | `min(10% × risk NAV, remaining external-release capacity)` |

Return, drawdown, and liquidation standards are dimensionless policy and do
not become looser as the vault grows. Their dollar amounts grow with NAV.
Premium, hedge-margin, and exercise caps are the lesser of their NAV budget
and measured venue capacity, so they stop growing when Bluefin, Sui spot, or
flash liquidity stops growing.

The policy denominator is conservative **risk NAV**, recomputed before every
quote:

```text
risk_nav = min(fresh complete appraised NAV,
               conservative locally reconstructed NAV)
         - queued-withdrawal liability
         - unresolved fill/PTB worst-case debits
         - required liquidity reserve
```

Live quote reservations are included once in each applicable premium
numerator; they are not also subtracted from `risk_nav`. If either NAV is
missing or stale, queued withdrawals cannot be valued, or external hedge
equity is stale, new quotes decline. A falling option mark may not create
capacity against a stale NAV.

The put-exercise threshold intentionally scales with the exercise slice and
route uncertainty, not whole-vault NAV. A small economically profitable put
must not become unexercisable merely because the vault grew. Route
uncertainty is the conservative cash bound for Pyth confidence/staleness,
router quote age, tick/rounding effects, and dev-inspect-to-inclusion drift.

---

## 1. Non-negotiable correctness requirements

Every item below has an automated gate.

1. **Production strategy parity.** Live trading and simulation use the same
   event-driven strategy kernel, quote logic, limits, exit policy, hedge
   policy, reservation state, and exact ledger.
2. **No option writing.** No strategy event or command may create a written
   option. Legacy written inventory may be detected for migration/unwind but
   is never part of the strategy.
3. **Signed hedging.** Positive perp units mean long; negative mean short.
   The neutral target is `-book_delta` for calls, puts, and mixed books.
4. **Direction-aware funding.** Funding is accrued against signed perp
   position. Positive market funding is income for shorts and cost for
   longs.
5. **No lookahead.** A decision uses only observations whose actionable
   time is at or before the decision time.
6. **Replay the bot's information set.** No historical Pyth capture exists
   and none will be collected (decision 2026-08-22). Historical pricing
   uses a proxy decision price built from collected/archive mids, degraded
   through an explicitly configured oracle model (latency, update cadence,
   confidence, staleness) before it reaches the strategy, and labeled
   `proxy_oracle=true` in every output. The strategy never sees exchange
   data faster or cleaner than the live Pyth path would have delivered it.
   Parity of the real Pyth gates is verified forward-only, in production.
7. **Determinism.** Same data, config, and seed produce byte-identical
   serialized output. Event ties, maps, reductions, RNGs, and parallel
   aggregation have deterministic ordering.
8. **Time does not stop in gaps.** Quote TTL, funding, expiry, margin, and
   pending-order exposure advance through missing-data windows.
9. **Exact accounting.** Cash, assets, liabilities, option marks, perp
   marks, reservations, margin, fees, and pending transactions reconcile to
   NAV after every event.
10. **Atomic exercise safety.** Every successful flash path repays exactly,
    returns residual assets to the vault, and clears a configured minimum
    profit. Failure aborts the whole PTB.
11. **Out-of-sample discipline.** Calibration, tuning, validation, and final
    holdout are chronologically separated. The final holdout is opened once.
12. **Uncertainty is visible.** Outputs include flow seeds, confidence
    intervals, sample counts, data coverage, queue assumptions, latency
    assumptions, and proxy-data labels.

---

## 2. Architecture: one kernel, two environments

The backtester must execute the desk's behavior, not a reimplementation of
it. Pure pricing-function parity alone is insufficient because reservations,
outstanding quotes, timers, order acknowledgements, transaction failures,
and fills are path-dependent.

Build a shared deterministic kernel:

```text
                    ┌────────────────────┐
 external event ───▶│ DeskKernel::on_event│
                    └─────────┬──────────┘
                              │ commands
                 ┌────────────┴────────────┐
                 ▼                         ▼
          live adapters             simulation adapters
      WS / Sui / Bluefin          clock / fills / failures
```

### 2.1 Events into the kernel

- Pyth market observation or staleness transition.
- Earn RFQ arrival.
- Quote sent, accepted, expired, reverted, or filled.
- Auction opened, bid placed, outbid, redeemed, or expired.
- Hedge order acknowledged, partially filled, filled, rejected, or
  cancelled.
- Funding settlement.
- Mark/margin update and liquidation.
- Holding/resale/expiry/exercise timer.
- Exercise PTB success or failure.
- Margin top-up success or failure.
- NAV sample and kill-switch transition.

### 2.2 Commands out of the kernel

- Quote or decline an RFQ.
- Reserve or release premium.
- Submit, cancel, or replace a hedge order.
- List or resell an option.
- Execute a call PTB.
- Execute one of the three put PTBs.
- Top up margin.
- Activate or clear a policy state such as a kill switch.

### 2.3 Exact ledger versus economic attribution

The exact ledger is the source of truth:

```text
NAV = settlement cash
    + spot inventory
    + exact option marks
    + perp collateral
    + perp unrealized P&L
    - outstanding liabilities
```

The attribution layer explains changes but never defines NAV. It reports:

- Protocol premium fee (`ProtocolConfig.fee_bps`, skimmed from gross
  premium on every write): a writer-side wedge, not a desk cost — the desk
  pays gross, the writer receives net, so it lowers the DISPLAYED APY the
  flow model sees (doc 09 G7).
- Curator performance fee and protocol share (trading vault): the split
  between the desk's gross return and the depositor-net return the hurdle
  is tested against.
- Model edge at entry.
- Realized option payoff/resale P&L.
- Delta, gamma, theta, vega, and basis contributions.
- Perp realized and unrealized P&L.
- Maker/taker fees, slippage, gas, and fixed costs.
- Funding by signed hedge direction.
- Exercise and liquidation costs.
- Higher-order/discretization residual.

The model edge line is explicitly not described as earned spread or
realized revenue.

---

## 3. Work that starts immediately and runs in parallel

These tasks accumulate otherwise unrecoverable calibration data and do not
wait for the backtester.

### 3.1 Record every RFQ outcome

`desk::history` currently samples state and P&L but does not persist the
complete RFQ funnel. Add a durable table for WS and auction flow:

```text
request_id, source, option_type, strike, expiry, size,
request_received_at, market_observed_at, model_fair,
quoted_premium, displayed_apy, surface_vol,
quote_sent_at, valid_until,
outcome, outcome_at, decline_or_revert_reason,
spot_at_request, spot_at_acceptance,
response_latency, fill_transaction
```

Every RFQ reaches exactly one terminal outcome:
`declined | expired | accepted | reverted | filled`.

This dataset calibrates separate call/put arrival and acceptance models,
including the value of stale quotes during their TTL.

**Decision (2026-09-01): no own-exchange data before mainnet.** Nothing
observed on our own exchange or Earn RFQ channel while they run on testnet
is calibration data, and none of it enters the data room. We are the only
participants, and even real testnet users would not behave like capital at
risk. The `desk_rfq_outcomes` table shipped in PR A is therefore
operational telemetry only; no backtest, flow prior, or acceptance model
may read it. Proper own-venue capture (exchange order flow and the Earn
RFQ funnel) is a mainnet-launch prerequisite to be designed separately;
until it exists, §8 runs on stated priors and sweeps.

Follow-up (decided 2026-09-01, not yet implemented): gate the PR A
recorder behind a config flag that defaults off, so testnet deployments
write no RFQ rows at all. Keep the table, migration, and code for mainnet.
Not a revert.

### 3.2 Continue live source collection

Collect and retain:

- Bluefin trades, L2, mark/ticker, funding, and venue status. Bronze runs
  since 2026-08-15; the S5 silver normalizer
  (`data-room/docs/l2-silver-schema-plan.md`) replays it when it lands.
- Bluefin funding settlements: add a REST funding-rate-history poller and
  derive settlement events in silver from the ticker stream's
  `nextFundingTimeAtMillis` rollovers; cross-check the two.
- Sui spot/router quote ladders in both directions. The current Aftermath
  ladder is sell-base only; add the mirrored buy-base rungs (an "S1c"
  poller config) and a `direction` column in the ladder silver schema
  before S5 freezes it.
- Chain inclusion and indexer-detection latency for exercise/fill events
  (not collected anywhere yet).
- Collector stall alerting is live (`services-data-room` scrape +
  `dataroom-collector-stalled`), but the rule aggregates per exchange;
  tighten it to per-stream so one dead poller cannot hide behind a live
  sibling stream.

Pyth capture is deliberately deferred (decision 2026-08-22): the strategy
is built on proxy decision prices and runs live against Pyth. Revisit only
if live-versus-backtest divergence demands it.

### 3.3 Make currently agreed strategy corrections

- Change the provisional call-band hypothesis from 1.5%/2.5% to 15%/25%,
  as required by doc 07. Put and mixed-book defaults remain backtest-gated.
- Define “quote every RFQ” as every eligible RFQ; stale data, hard size,
  solvency, kill-switch, and unsafe-execution cases decline explicitly.
- Separate call and put premium, delta-direction, gamma, expiry, and
  exercise-liquidity reporting.

### 3.4 Current trading-vault wiring baseline

The existing desk is already vault-native for option acquisition and
custody:

- Boot resolves or provisions one `TradingVault`, verifies that it is open
  and denominated in the settlement asset, proves `CuratorCap` ownership,
  and enables the curator's `vault_mm` release gate.
- Every signed Earn quote names the vault as `collateral_source`, calls
  `vault_mm::release`, and names the vault address as the option recipient.
- Filled option coins are swept into vault position custody, appraised, and
  included in share price/NAV.
- The contract has a separate NAV-relative external-account budget and
  daily release limit suitable for Bluefin hedge margin.

The present off-chain policy is not sufficient for the strategy in this
document:

- NAV limits use the latest indexed PPS times shares without a quote-time
  freshness gate or queued-withdrawal valuation.
- Premium limits are total-book only; there are no call, put, or effective
  exercise-capacity sublimits.
- Signed-quote reservations live only in process memory and disappear on
  restart.
- Hedge execution is paper-only and short-only; the external vault account
  is not yet used by the desk.
- Funding is estimated from option premium rather than signed hedge
  notional.
- Put acquisition is priceable, but the current exit ladder always holds
  puts and the current hedge cannot open the required long perp.
- Existing flash call exercise requires only one unit of positive residual;
  it does not enforce the vault-scaled safety threshold above.

The framework treats these as production-parity gaps, not simulator-only
features.

---

## 4. P0 — production strategy correctness

The framework does not begin by simulating known-broken production
semantics. P0 makes the single long-option strategy internally complete.

### 4.1 Remove the option-writing strategy

Remove or permanently disable:

- Trader-flow option-writing quotes.
- V2 quote/skew parameters.
- Naked-short budgets and short-option stress gates.
- Strategy commands that acquire collateral and write options.

Written-position readers may temporarily remain for legacy detection and
unwind. Add a kernel invariant that no new written position can be created.
The testnet counterparty simulator (`services/mm-bot/src/sim.rs`) plays the
Earn user and *writes* covered calls at the desk; it is out of scope for
this removal and must keep building.

Gate:

- Every trader-side RFQ declines with a stable reason.
- No strategy command can create written inventory.
- Legacy written inventory is surfaced explicitly and blocks normal trading
  until handled.

### 4.2 Replace the short-only hedge model with signed positions

Use one convention everywhere:

```text
perp_position > 0  => long perp
perp_position < 0  => short perp
target_perp         = -book_delta
net_delta           = book_delta + perp_position
```

The venue interface is order/event-oriented, not synchronous
`adjust_to(...) -> Result<()>`:

```rust
enum HedgeCommand {
    Submit(HedgeOrder),
    Cancel(OrderId),
    Replace { old: OrderId, new: HedgeOrder },
}

enum HedgeEvent {
    Acknowledged(OrderId),
    PartiallyFilled(Fill),
    Filled(Fill),
    Rejected { order: OrderId, reason: String },
    Cancelled(OrderId),
}
```

Support long, short, reduce, close, direction reversal, partial fill, and
fill-after-cancel races. The paper venue adopts the same signed semantics.

Gate:

- A long-call fixture targets a short perp.
- A long-put fixture targets a long perp.
- Equal and opposite deltas net without a trade.
- Direction reversal realizes P&L correctly.
- Call-only behavior is unchanged apart from the sign convention.

### 4.3 Make quote hedge costs direction-aware

Do not pass a raw “short receives” funding rate into option pricing. Resolve
quote-specific expected hedge cost before calling the bid function:

```rust
struct ExpectedHedgeCost {
    funding: f64,
    venue_fees: f64,
    slippage: f64,
    fixed_cost: f64,
}
```

For each proposed fill:

1. Calculate incremental option delta.
2. Calculate the required signed perp change.
3. Forecast funding over the expected holding period.
4. Convert it to an expected cash cost using position sign.
5. Add venue fees, slippage, and fixed costs.
6. Subtract the nonnegative total from the bid.

Funding and margin financing are charged on incremental signed hedge
notional, not option premium:

```text
incremental_hedge_notional = abs(delta_after_fill - delta_before_fill) × spot

expected_hedge_cost = signed_funding_cost(incremental_hedge_notional, holding_time)
                    + margin_financing_cost(required_incremental_margin, holding_time)
                    + expected_hedge_turnover × (fees + slippage)
                    + transfer and fixed costs

capital_charge = return_hurdle × incremental_risk_capital × holding_time

maximum_bid = model_fair
            - model_edge_buffer
            - expected_hedge_cost
            - expected_exercise_cost
            - capital_charge
```

`incremental_risk_capital` includes premium, quote reservation, initial
margin, stressed top-up reserve, and exercise-liquidity allocation, with
explicit credit for call/put delta netting. This creates the price/volume
frontier: the desk may support more Earn volume only when the writer accepts
a bid that still clears the vault's return hurdle.

Funding income is reported as upside or handled by an explicitly configured
conservative credit; it never appears accidentally because the quote code
assumed a short hedge.

Gate:

- Positive funding is income for a short call hedge.
- Positive funding is cost for a long put hedge.
- Negative funding reverses those outcomes.
- A delta-net mixed fill has approximately zero incremental funding cost.
- Hedge funding is proportional to hedge notional, not premium paid.
- A quote that consumes scarce margin or expiry capacity receives a larger
  capital charge or declines at the hard limit.

### 4.4 Complete the put exercise policy and PTBs

For `q` puts, strike payout is `K × q` settlement. Exercise only when:

```text
strike payout
- underlying acquisition or replacement cost
- swap fees and slippage
- flash cost
- gas
>= configured minimum profit
```

Choose the first profitable available atomic PTB:

1. **Vault underlying:** deliver vault underlying, receive strike payout,
   use part of the payout to repurchase the delivered amount, keep residual
   settlement.
2. **Base flash loan:** borrow underlying, exercise, use payout to acquire
   the exact underlying repayment, repay, keep residual settlement.
3. **Quote flash loan:** borrow settlement, buy the required underlying,
   exercise, repay settlement from the strike payout, keep the residual.

Each path uses dev-inspect/pre-simulation, explicit min-output/max-input,
exact repayment, pool allowlists, flash-capacity checks, gas bounds, and a
minimum-profit assertion. Failure aborts atomically. Large positions are
laddered without crossing expiry.

The Sui PTB is atomic; the Bluefin long-perp unwind is not. After PTB
success, schedule the signed hedge close immediately and model the delay and
basis risk separately.

Gate:

- All three paths succeed independently.
- Unavailable base liquidity falls through to quote flash.
- Slippage, capacity, repayment, or profit-bound failures abort atomically.
- No option, loan, or residual asset is stranded.
- Partial and laddered exercise reconcile to the ledger.
- A redundant keeper exercises every economically exercisable put before
  expiry.

### 4.5 Add composition-aware risk limits

In addition to total vega, theta, and premium, compute and expose:

- Call premium and put premium.
- Positive-delta and negative-delta inventory.
- Gamma by option type and expiry.
- Crash loss on long-perp put hedges before monetization.
- Rally loss on short-perp call hedges before monetization.
- Call settlement cash required by expiry.
- Put underlying required by expiry.
- Base-flash and quote-flash utilization by expiry.
- Concurrent exercise and margin-top-up demand.

Gate: call-heavy, put-heavy, and mixed books all exercise the intended soft
throttles and hard solvency declines.

### 4.6 Derive an effective vault capacity snapshot

The current desk scales total premium, vega, theta, and expiry limits from
`latest_pps_e12 × total_shares`. Replace that single number with a fresh,
auditable `CapitalSnapshot` shared by live quoting and simulation:

```text
appraised_nav, locally_reconstructed_nav, risk_nav,
free_settlement, free_underlying_by_asset,
queued_withdrawal_value,
call_premium_marked, put_premium_marked,
call_quote_reservations, put_quote_reservations,
premium_by_expiry,
external_exposure, external_equity,
external_budget_remaining, external_daily_release_remaining,
venue_initial_margin, venue_maintenance_margin, venue_margin_headroom,
base_flash_capacity, quote_flash_capacity,
spot_buy_capacity_by_slippage, spot_sell_capacity_by_slippage,
observed_at, appraisal_at, external_equity_at
```

From that snapshot compute:

- `effective_call_capacity`: minimum of the call premium budget, short-perp
  margin capacity, call-exercise settlement/sale capacity, and free quote
  cash.
- `effective_put_capacity`: minimum of the put premium budget, long-perp
  margin capacity, the three-path put exercise capacity, and free quote
  cash.
- `effective_expiry_capacity`: minimum of the per-expiry premium budget,
  concurrent exercise capacity, and stressed margin/top-up capacity for that
  expiry.

No on-chain pool-balance poller exists yet, so `base_flash_capacity` and
`quote_flash_capacity` are configured assumptions in backtests (decision
2026-08-22): runs assume flash capacity is available, label the
assumption, and report whenever it binds. If live flash capacity proves
insufficient, the response is a better flash venue, not a strategy
constraint. Live quoting reads real balances once the poller exists.

Reservations are durable and keyed by quote/request id with explicit
`quoted → accepted/reverted/expired/filled` transitions. Restarting the bot
must reconstruct every still-live reservation from durable state and chain
events. The process-local TTL map is not sufficient for a vault-backed
maker.

The trading vault already provides a NAV-relative external account budget
and 24-hour release limit for hedge margin. The policy engine consumes the
on-chain remaining values; it does not assume configured Bluefin margin is
available. Under the curator self-serve path these are capped at 20% and 10%
of appraised NAV respectively, so a larger hedge allocation requires an
explicit admin-reviewed vault configuration.

Gate:

- Every derived dollar cap rises and falls with fresh risk NAV.
- A fixed venue/flash capacity eventually binds as NAV grows.
- Stale appraisal, external-equity, or withdrawal data blocks new risk.
- Call, put, total, and per-expiry reservations cannot double-spend the same
  cash.
- Restarting during live quotes preserves the same available capacity.

---

## 5. P1 — workspace merge, shared kernel, and exact ledger

### 5.1 Merge `data-room` into the Rust workspace

Move `data-room/` under `rust-backend/` so the backtester can share pricing,
strategy, store, and market-data crates. Preserve runtime isolation:

- Do not add data-room binaries to protocol compose/deploy sets.
- Add a CI rule preventing collectors from importing protocol service
  clients or touching RDS.
- Fix workflow triggers, Docker build paths, CI/fmt/clippy, Cargo.lock, and
  documentation paths.

Arrow, Parquet, object_store, and reqwest versions already match. Known
merge mechanics that must be handled (surveyed 2026-08-22):

- `metrics-exporter-prometheus` conflicts (data-room 0.16 with
  `http-listener`, rust-backend 0.17 without): unify on 0.17 and add the
  feature; the collector's `:9100` endpoint must keep serving.
- Tokio drops from 1.53 to the workspace's `=1.49.0` sui-keys pin;
  untested.
- `tokio-tungstenite` must keep `rustls-tls-webpki-roots`; add `flate2`,
  `zip`, `bytes`, and `tempfile` to workspace dependencies.
- The data-room Dockerfiles `COPY . .` with `data-room/` as build context;
  after the move they need scoped contexts or the collector image build
  pulls the whole protocol workspace, Sui git deps included.
- `deployment/affected.py` treats `rust-backend/Cargo.lock` as
  rebuild-all: carve data-room members out, or every data-room dependency
  bump rolls all protocol services. `deploy-filter-ci.yml` needs matching
  cases.
- Scope data-room CI with `-p` flags; a bare `cargo clippy --workspace`
  would compile the full Sui dependency tree.
- Rename the generic crates (`schema`, `store`, `adapters`) with a
  data-room prefix, and decide whether data-room's thin-LTO release
  profile applies workspace-wide or is dropped.

### 5.2 Extract `rust-backend/crates/desk-core`

Move pure state and policy; keep runtime adapters in `services/mm-bot`:

| Moves to `desk-core` | Remains in `mm-bot` |
|---|---|
| `DeskKernel`, events, commands | WS/Sui/Bluefin adapters |
| Quote inputs and writer-flow decision | Authentication and signing |
| Limits and kill-switch policy | Chain/indexer readers |
| Market model and rolling-vol adapter | Provisioning and service state |
| Holdings, reservations, exact ledger | Database history recorder |
| Call and put exit policy | PTB construction/submission |
| Signed hedge policy | Venue connections and task scheduling |

Keep `RollingVolBuffer` as the exact shared implementation; do not recreate
its estimator in the backtester.

### 5.3 Build the exact ledger

Track at minimum:

- Settlement and underlying balances.
- Long call and put positions.
- Signed perp units, entry basis, collateral, realized and unrealized P&L.
- Outstanding RFQ reservations by request id, option type, and expiry.
- Queued withdrawal shares and their current settlement-value liability.
- External-account exposure, attested equity, total budget, daily release
  usage, and data freshness.
- Pending quote, hedge, margin, resale, and exercise operations.
- Funding, fees, gas, slippage, penalties, and flash liabilities.

Ledger invariants:

- Assets minus liabilities equal NAV after every event.
- Reservations plus committed spend never exceed available capital.
- Call, put, total, and expiry-level premium usage reconcile to the same
  underlying reservations and holdings.
- Flash liabilities are zero after a successful PTB.
- A failed atomic PTB changes no ledger balance.
- Exercise removes the exact option quantity and delivers the exact assets.
- Perp realized plus unrealized P&L reconciles across fills and reversals.

### 5.4 Remove deprecated simulators safely

Delete `rust-backend/crates/vault-sim` and the deprecated
`rust-backend/tools/backtester` only after checking the `cursor.rs` and
`ledger.rs` Move goldens shared with `vault_tests.move`. The keeper also
depends on vault-sim: `services/keeper/tests/strike_goldens.rs` imports
`vault_sim::strategy::StrikeSelector` to golden-check `keeper::strike`.
Decision 2026-08-22: cut the legacy covered-call vault support out of the
keeper along with the simulators, rather than re-homing the strike
goldens. Preserve the remaining shared golden behavior.

P1 gate:

- Workspace CI, collector image, and batch image pass.
- Live adapter tests and simulation adapter tests produce identical commands
  for identical event traces.
- Hand-computable call, put, expiry, exercise, hedge reversal, funding, and
  liquidation ledger fixtures pass.
- `/desk/state` config changes are intentional and migration-tested; a
  signed-funding config change is not incorrectly required to be
  byte-identical to the old short-only schema.

---

## 6. P2 — causal replay and clock

### 6.1 Replay distinct information domains

Do not collapse market truth and the bot's observations into one price.

| Domain | Source | Use |
|---|---|---|
| Strategy decision price | Oracle proxy: collected/archive mids degraded through the configured oracle model (no Pyth history exists) | Quotes, Greeks, vol sampling, limits |
| Spot execution | Sui router/DeepBook | Exercise acquisition and unwind |
| Perp execution | Bluefin L2/trades | Hedge fills |
| Risk mark | Bluefin mark | Unrealized P&L and liquidation |
| Funding | Bluefin settlements | Signed funding cash flow |
| Proxy/stress | Binance archives | Long-history sensitivity only |

The production Pyth fields (confidence, publish time, receive time,
observed time, staleness, settlement leg) are synthesized by the
oracle-proxy model with stated assumptions; parity of the real gates is
verified forward-only in production. SUIUSDT is not silently treated as
SUI/USDC; stablecoin basis/depeg is explicit.

### 6.2 Use explicit timestamp types

```text
EventTime          venue/source event
ReceiveTime        collector/oracle receipt
ActionableTime     available to the strategy
CommandTime        strategy submits action
AcknowledgementTime venue accepts action
FillTime           execution occurs
ChainInclusionTime Sui transaction finalizes
DetectionTime      indexer/book observes result
```

Define deterministic tie order:

1. External events occur.
2. Events become observable after feed latency.
3. Timer and RFQ strategy events run against the observable cache.
4. Commands are submitted.
5. Acknowledgements, fills, and chain results arrive.
6. Ledger and book state update.

Timer events include vol samples, hedge samples, exit checks, quote expiry,
funding settlement, margin checks, expiry, kill-switch sampling, and retry
timers. They are merged into the event stream, never scheduled from wall
clock.

### 6.3 Model latency by stage

A single global offset is insufficient. Configure/sweep separate latency
distributions for:

- Pyth/market observation.
- Strategy computation and quote response.
- Customer acceptance.
- Bluefin submission, acknowledgement, cancel, and fill reporting.
- Sui transaction inclusion.
- Indexer/fill detection.

Archive rows without receive timestamps use explicitly labeled assumed
latencies. BTC collector overlap may calibrate collector delay but is not
silently transferred to Pyth, Bluefin, or Sui execution.

### 6.4 Treat gaps as uncertainty, not frozen time

Each run declares required feeds. During a required-feed gap:

- All timers continue.
- Cached data ages and production staleness gates fire normally.
- Existing quotes and resting orders remain exposed.
- Funding, margin, expiry, and pending transactions continue.
- Unknown fills/liquidations are either bounded conservatively or the span
  is invalidated.

Outputs contain coverage, gaps, invalidated spans, and any optimistic or
conservative bounds. A run never earns by assuming nothing adverse happened
inside a capture hole.

### 6.5 Preserve bounded memory

Reuse `gold/read.rs` streaming readers and reduced series (today they
cover trades and book-top mids only; readers for funding, ladders, and L2
are net-new). Build a multi-table k-way event merge over one Arrow batch
per source. The 200 ms
reduced series is the floor; sub-200 ms execution uses raw rows.

P2 gate:

- Replay a known week with source row counts reconciled independently.
- Timer counts and event ordering are stable.
- Pyth staleness and confidence declines match live behavior.
- A gap fixture advances TTL/funding/expiry and never freezes risk.
- Same inputs and seed serialize byte-identical output.

---

## 7. P3 — execution, margin, and exercise models

### 7.1 Earn quote lifecycle

An outstanding signed quote is a free option for the user during its TTL.
Represent it explicitly:

```text
RFQ arrival
→ response latency
→ quote sent and premium reserved
→ acceptance hazard over the remaining TTL
→ chain inclusion or revert
→ fill detection or expiry
```

Acceptance uses the market path while the quote remains live. Capital is
reserved until the actual terminal event, not merely for a synthetic fixed
delay.

### 7.2 Bluefin execution lifecycle

Implement:

- Taker/market fills.
- Passive placement and acknowledgement.
- Queue-ahead assumptions.
- Partial fills.
- Cancel/replace latency.
- Fill-after-cancel races.
- Contract rounding, fees, and fixed costs.
- Persistent own-order impact and consumed depth.

Market-by-price L2 cannot recover exact queue position. Every result names
one execution assumption:

```text
optimistic | central | conservative | taker-only
```

Passive-fill fraction remains a sensitivity axis, not a substitute for the
order lifecycle. Passive-fill parameters may be calibrated only from the
native capture that starts 2026-08-15; in the proxy-BBO era (§10) passive
results are sensitivity-only, never calibrated fact.

### 7.3 Margin and liquidation

Implement verified Bluefin rules:

- Initial and maintenance margin.
- Position/leverage tiers.
- Mark-based unrealized P&L.
- Margin top-up amount and transfer latency.
- Partial/full liquidation and penalties.
- Contract rounding and venue caps.
- Venue outage and rejected top-up behavior.

The acute market risk exists in both directions: short call hedges lose in
rallies and long put hedges lose in crashes while option gains remain
unavailable as margin.

Binance mark/funding runs are `proxy_venue=true`. They size stress but do not
prove Bluefin survival.

### 7.4 Funding and basis

Accrue historical funding against signed position at each venue settlement.
Use spot for options, perp execution for hedge fills, and mark for margin.
At exercise, model basis and the delay between the atomic Sui PTB and the
non-atomic Bluefin hedge close.

### 7.5 Call exercise

Model cash-first and quote-flash call exercise, spot sale, exact repayment,
profit threshold, route selection, laddering, and subsequent short-perp
close. Flash capacity and swap depth are distinct constraints.

### 7.6 Put exercise

Simulate the same three-path policy used live:

- Vault underlying and replacement.
- Base flash, replacement, and exact base repayment.
- Quote flash, underlying acquisition, exercise, and exact quote repayment.

Model pool balance capacity, route depth, min output/max input, gas, PTB
failure, fallback between paths, and the delayed long-perp close.

P3 gate:

- Taker fills reconcile exactly against hand calculations.
- Passive order fixtures cover partial fill, cancel race, and no-fill cases.
- Funding matches historical settlements for long and short positions.
- Margin fixtures match verified venue equations.
- Historical crash/rally proxies use marks rather than trades.
- Every call and put exercise path selects the same route as live policy and
  reconciles to the exact ledger.

---

## 8. P4 — Earn flow generator

Flow is the only fully synthetic economic input. Calls and puts use separate
models and stated priors. There is no historical RFQ dataset to replay, so
the initial framework reports scenario-conditioned capacity rather than a
forecast of customer demand.

### 8.1 Define volume precisely and run two modes

Every result distinguishes:

- **Offered Earn notional:** underlying spot notional submitted by writers.
- **Quoted Earn notional:** offered notional that passed hard eligibility
  checks and received a bid.
- **Accepted Earn notional:** notional actually bought by the vault.
- **Premium turnover:** settlement premium paid to writers.
- **Hedge turnover:** absolute perp notional traded, including rebalances.
- **Exercise spot turnover:** underlying bought or sold during exercise.

Run two complementary modes:

1. **Capacity mode:** inject a target accepted Earn volume independent of
   demand elasticity. Solve the minimum starting NAV that can service it
   without violating cash, premium, expiry, hedge, exercise, drawdown, or
   liquidation gates.
2. **Market mode:** generate offered writer flow and acceptance against the
   actual bid returned by the strategy. This estimates attainable volume at
   the return hurdle, conditional on stated demand assumptions.

The default offered-volume sweep is logarithmic and configurable, for
example `$10k, $25k, $50k, $100k, $250k, $500k, $1m, $2.5m, $5m, $10m`
of spot notional per day. Each level runs call-only, put-only, balanced,
and adversarial mixes. A result never uses the ambiguous label “volume”
without one of the definitions above.

### 8.2 Arrival models

Condition call and put arrivals separately on:

- Trailing return and direction.
- Volatility regime.
- Displayed strike/tenor menu.
- Moneyness.
- Displayed premium APY.
- Collateral type and alternative yield.
- Time of day and expiry calendar.

Call writing may increase after run-ups; put writing may increase after
sell-offs or volatility spikes. These are hypotheses until mainnet RFQ
outcomes calibrate them; testnet outcomes never do (§3.1 decision).

### 8.3 Size and bucket selection

Use separate heavy-tailed size distributions for calls and puts, bounded by
protocol limits and plausible writer collateral. Model option type, strike,
expiry, and synchronized bucket concentration jointly.

### 8.4 Acceptance during TTL

Acceptance is a hazard over the quote lifetime, not a one-time logistic
draw:

```text
P(accept at t | quoted premium,
                  displayed APY,
                  current option value,
                  moneyness,
                  size,
                  option type,
                  time remaining)
```

This captures selection into stale quotes after favorable spot moves.
Without price elasticity, widening is free money and spread sweeps are
invalid.

### 8.5 Resale

Default is no resale and hold/exercise. Resale is a separately labeled
upside scenario with its own call/put demand, fill probability, price, and
latency assumptions.

### 8.6 Capital-to-volume solver

For accepted daily spot notional `V`, the first-order inventory estimate is:

```text
average outstanding option notional ≈ V × average holding days
premium at risk                      = Σ option premium marks + live reservations
incremental hedge notional           = abs(net delta after fill - net delta before fill) × spot
```

The simulator remains the source of truth because expiry clustering,
acceptance timing, hedge bands, and call/put delta netting make capital use
path-dependent. For each target volume, binary-search starting NAV until at
least 95% of flow seeds service the target without a capacity decline and
all seeds have zero liquidations. The following lower-bound diagnostic must
agree with the simulated binding constraint:

```text
required_nav >= max(
    total_premium_at_risk / 0.30,
    call_premium_at_risk / 0.20,
    put_premium_at_risk / 0.20,
    peak_expiry_premium_at_risk / 0.10,
    required_external_margin / external_budget_fraction,
    peak_24h_margin_topup / external_daily_release_fraction,
    historical_loss / 0.15,
    synthetic_stress_loss / 0.25
)
```

That bound is necessary but not sufficient. The candidate NAV must also
have enough free settlement for accepted premiums and reservations, enough
fresh Bluefin margin capacity, and enough on-hand/flash/router capacity for
the exercise schedule. External budget fractions come from the live vault;
they are not assumed to be 20%/10% when governance configured lower limits.

For every target volume, output:

- Minimum starting NAV and the 95% confidence interval across flow seeds.
- Binding constraint and next two constraints.
- Call, put, total, and peak-expiry premium at risk.
- Peak and average free settlement consumed by quotes.
- Initial hedge margin, maximum top-up, minimum margin headroom, and
  external-account budget usage.
- Hedge turnover, funding, fees, slippage, and capital charge embedded in
  the bid.
- Call/put exercise path, flash utilization, router utilization, and
  failed/laddered exercise count.
- Quoted, accepted, and declined notional with decline reasons.
- Net return, drawdown, liquidation count, and return-hurdle pass/fail.

Plot two frontiers rather than one headline APY:

```text
minimum NAV required ↔ accepted Earn volume
maximum sustainable bid/APY ↔ attainable Earn volume
```

If more volume requires bids that fail the return hurdle, report that point
as demand-limited or uneconomic, not as missing vault capital.

### 8.7 Randomness discipline

- Same seed produces identical flow and acceptance.
- Parameter variants use common random numbers.
- Each result includes accepted RFQs, expiries, option type counts, and
  effective capital deployed.
- Report distributions across seeds, not the best seed.

P4 gate:

- Capacity mode recovers hand-calculated constant-flow fixtures.
- Doubling volume approximately doubles required capital in a non-netted,
  unconstrained fixture.
- Venue or flash capacity creates a visible nonlinear ceiling as NAV grows.
- Wider bids reduce acceptance.
- Favorable stale quotes are accepted more often.
- Unfavorable quotes expire more often.
- Call and put models respond differently to return direction and APY.
- No-resale results complete successfully.

---

## 9. P5 — attribution, sweeps, and validation

### 9.1 Output both accounting and explanation

Per turn, regime, option type, seed, and cumulatively report:

- Exact realized cash return and annualized return.
- Model edge at entry, explicitly non-realized.
- Option payoff/resale P&L.
- Perp realized/unrealized P&L.
- Funding by long/short direction.
- Maker/taker fees, slippage, gas, and fixed costs.
- Exercise cost and selected PTB path.
- Liquidation loss and minimum margin headroom.
- Delta/gamma/theta/vega/basis explanation and residual.

The exact ledger must reconcile. The Greek explanation may retain a bounded
higher-order residual, especially across gaps.

Because core replay uses a closed NAV, net annualized return is the CAGR of
exact beginning and ending NAV after funding, fees, gas, slippage, exercise,
liquidation, and idle-cash opportunity cost. Capacity runs also report net
profit per accepted Earn notional and return on incremental risk capital so
high volume cannot hide poor capital efficiency.

### 9.2 Walk-forward protocol

Split history chronologically:

1. **Calibration:** fit flow, latency, and execution priors.
2. **Walk-forward training:** choose parameters using only past data.
3. **Validation:** test robustness and freeze the strategy/config family.
4. **Final holdout:** open once against predeclared success criteria.

Never rank configurations on the final holdout.

### 9.3 Required portfolio variants

- Call-only, to reproduce doc 07.
- Put-only.
- Balanced mixed book.
- Historically calibrated mix.
- Adversarial call-heavy and put-heavy mixes.

### 9.4 Primary sweep axes

- Offered and target accepted Earn notional per day.
- Call/put mix.
- Writer-size and expiry-clustering distributions.
- Writer acceptance elasticity and outside-yield assumptions.
- Call and put bid spread/skew adjustments.
- Hedge band width and wide-band threshold.
- Maker/taker/passive execution assumption.
- Tenor and bucket schedule.
- Holding period/resale assumption.
- Premium budget and call/put sublimits.
- Observation, order, acceptance, and chain latency.
- IV estimator multiplier and surface regime.
- Call/put arrival and acceptance parameters.
- Margin buffer and top-up policy.

Output break-even surfaces, not a single optimized number.

### 9.5 Synthetic stress suite

Historical replay is insufficient for unobserved but plausible failures.
Run:

- Instant -60% and +80% gaps.
- Multi-step crash/rally with delayed Pyth updates.
- Six-month flat market.
- Volatility collapse immediately after purchases.
- Funding +/-50% annualized for 30 days.
- Bluefin outage during exercise and margin stress.
- Sui congestion near expiry.
- No resale.
- No base flash liquidity.
- No quote flash liquidity.
- Router depth collapse.
- Concentrated synchronized expiry.
- Settlement stablecoin depeg.

### 9.6 Statistical output

Report:

- Minimum NAV and binding capital constraint at every target volume.
- Offered, quoted, accepted, premium, hedge, and exercise volume separately.
- Return-versus-volume and capital-versus-volume efficient frontiers.
- Mean, median, quantiles, and confidence interval across flow seeds.
- Maximum drawdown and CVaR.
- Liquidation count/probability and closest margin headroom.
- Accepted RFQ count and independent expiry count.
- Results by market regime and option type.
- Data coverage and invalidated spans.
- Proxy venue, queue, latency, and resale assumptions.
- Sensitivity to nearby parameters, not just the optimum.

P5 gate:

- Doc 07 call turnover/cost results reproduce within stated tolerance.
- Put and mixed fixtures have independent hand checks.
- Hand-calculated capital/volume fixtures match the capacity solver.
- Every target-volume result identifies whether it is demand-limited,
  capital-limited, venue-limited, or uneconomic at the return hurdle.
- No parameter is selected using future or holdout data.
- Central and conservative execution results are both published.
- The final holdout remains sealed until product thresholds are fixed.

---

## 10. Data dependencies and truth labels

Standing decisions (2026-08-22): free data only — no vendor tick-history
purchases; no Pyth collector; DeepBook flash capacity is assumed for
backtesting (§4.6); the Deribit/DVOL options history is used as-is.

| Need | Status / rule |
|---|---|
| Historical SUI spot/perp trades and funding | Binance Vision available/in progress; always label as proxy where Bluefin-specific behavior matters |
| Pyth underlying + settlement history | Does not exist and will not be collected; the decision price is an oracle proxy (§6.1) labeled `proxy_oracle=true`, and live Pyth parity is forward-only |
| SUI mark history | Binance premium-index archives support proxy stress; Bluefin mark bronze accumulates since 2026-08-15 and is required for venue claims |
| Bluefin L2, trades, mark/ticker, funding, venue status | Bronze accumulating since 2026-08-15; silver is S5 (planned, not started); funding settlements need the §3.2 poller/derivation; calibration depth grows with the capture window |
| Sui router/DeepBook bidirectional quote ladder | Sell-base rungs accumulating since 2026-08-15; the buy-base direction (put exercise) is not collected yet — see §3.2 |
| DeepBook base/quote pool balances | No poller exists; backtests assume configured flash capacity (§4.6) and label it; if live capacity binds, switch flash venue |
| L2 silver schema | `data-room/docs/l2-silver-schema-plan.md` |
| Historical SUI BBO | Binance `bookTicker` 2023-05 to 2024-04 only; the window predates both the doc 07 measurement regime and the 2025-10-10 cascade, and no free replacement exists — proxy-era passive fills are sensitivity-only (§7.2) |
| Staking yield | Shared config initially; historical series later; affects call and put pricing/exercise |
| Settlement cash yield | Policy/config series; sets the return hurdle floor and idle-cash opportunity cost |
| USDT/USDC or settlement basis | Required when proxy market prices and vault settlement differ |
| BTC DVOL + Deribit chain | DVOL hourly from 2021; the strike-level chain only from 2026-08-14, so the ablation runs on ATM DVOL (no skew/term) until the chain accumulates; not proof of SUI fair value |
| Historical Earn RFQ outcomes | Unavailable; never implied or synthesized as observed history |
| New live RFQ outcomes | Recorder shipped (PR A) but testnet rows are telemetry only, never calibration (§3.1 decision, 2026-09-01); mainnet capture of the RFQ funnel and exchange order flow is a launch prerequisite |

Every run serializes a data manifest containing source, venue, coverage,
version, gaps, proxy status, and hashes/partitions used.

---

## 11. Delivery sequence and parallel work

```text
P0 production correctness
    signed hedge + funding + put exercise + risk limits
                    │
                    ▼
P1 shared kernel and exact ledger ◀── workspace merge
                    │
                    ▼
P2 causal replay and clock ◀───────── collectors + RFQ logging
                    │
                    ▼
P3 execution, margin, exercise
                    │
                    ▼
P4 Earn flow, quote lifecycle, and capital-to-volume solver
                    │
                    ▼
P5 walk-forward sweeps + stress + final holdout
```

Recommended PR boundaries:

| PR | Scope | Gate |
|---|---|---|
| A | RFQ outcome history | Every RFQ has one terminal outcome |
| B | Remove/disable option-writing strategy | No command creates written inventory |
| C | Signed hedge convention and paper venue | Call, put, mixed, reversal tests |
| D | Direction-aware expected hedge cost | Funding-sign quote matrix green |
| E | Put exercise policy + three PTBs | Atomic path/fallback/failure tests |
| F | Composition-aware limits | Call-heavy/put-heavy stress gates |
| G | Vault-scaled `CapitalPolicy` + durable reservations | Freshness, restart, and capacity gates |
| H | Workspace move | CI and images green |
| I | `desk-core` kernel extraction | Live/sim trace parity |
| J | Exact ledger | Accounting fixtures reconcile |
| K | Replay/clock/gap policy | Known-week and gap gates |
| L | Signed perp execution + margin | Venue fixtures and proxy labels |
| M | Exercise execution model | All live/sim route decisions match |
| N | Earn flow + capital-to-volume solver | Elasticity, capacity, and stale-quote tests |
| O | Attribution and walk-forward runner | Reproduction and holdout discipline |

RFQ logging and data collection run from day one. The sweep runner is last;
building it earlier would optimize against incomplete accounting and
unrealistic fills.

The replacement backtester lives in `rust-backend/crates/backtester` with a
thin binary under `rust-backend/tools/backtester`; library components remain
unit-testable.

---

## 12. Definition of validated

Do not call the strategy validated until all are true:

1. Exact ledger reconciliation passes every event and full replay.
2. Live and simulation adapters produce identical commands for identical
   event traces.
3. The strategy cannot create written options.
4. Calls and puts both quote, reserve, hedge, resell, expire, and exercise
   correctly.
5. All three put PTBs and their fallback order pass atomic failure tests.
6. No-resale mode completes and is economically survivable.
7. Results clear the predeclared return hurdle on the untouched holdout.
8. The lower confidence bound, not only the mean, clears the chosen hurdle.
9. Agreed historical and synthetic stresses remain inside drawdown and
   liquidation limits.
10. Margin top-ups remain feasible without violating premium/liquidity
    constraints.
11. Results remain acceptable across call-heavy, put-heavy, and mixed flow.
12. Profit does not depend on one latency, queue, IV, resale, or flow-seed
    assumption.
13. Capacity is bounded by measured hedge depth, flash balances, router
    depth, and expiry concentration.
14. Every target Earn volume has a minimum-NAV estimate, confidence interval,
    binding constraint, and economic/demand feasibility label.
15. Model edge is never presented as realized revenue.
16. Every published result includes uncertainty, data coverage, and proxy
    labels.

Until these gates pass, the framework reports break-even conditions and
failure surfaces, not a claim that the fund would have earned a historical
return.
