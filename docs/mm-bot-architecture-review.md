# MM-Bot Architecture Review: The Path to a Professional Options Market Maker

Status: review / design doc — no implementation. Written against
`rust-backend/services/mm-bot` as of `0a27274`.

The question this doc answers: **where is the mm-bot today, and what has to be
built — math and infrastructure — to turn it into a true options market maker
that hedges its book (delta first, then every greek), provides continuous
liquidity to the call and put markets, and is generalized to execute spot
hedges over any venue (DeepBook, swap routers, CEX, other chains)?**

---

## 1. Executive summary

The mm-bot today is a **quoting engine, not a market maker in the risk sense**.
It has four well-built quoting/bidding surfaces sharing one clean, well-tested
Black-Scholes pricing brain — but it has **no record of what it has traded, no
view of the risk it is carrying, no hedging of any kind, and no PnL**. Fills
are fire-and-forget: the bot signs a quote or wins an auction, tokens and
premiums land in its wallet/collateral account, and nothing downstream ever
looks at them as *positions*. Inventory management is literally "mint more from
the test faucet when low" (`main.rs::spawn_replenish_task`), which on testnet
deliberately papers over exactly the problem a real MM exists to manage.

The good news: the hard leaf-level math already exists and is high quality.
`crates/pricing` has analytic greeks for calls and puts (`call_greeks`,
`put_greeks`), an implied-vol solver, assignment probabilities, and a
parametric smile — all bump-and-reprice tested. The missing layers are the
classic middle of an options desk, in dependency order:

1. **Trade capture + position book** — you cannot hedge what you cannot see.
2. **Portfolio risk engine** — aggregate greeks per underlying, marked live.
3. **Delta hedging engine** — band-based spot hedging through a venue
   abstraction.
4. **Inventory-aware quoting + portfolio risk limits** — skew quotes to shed
   risk; cap what one book can carry; kill switch.
5. **Venue generality** — an `ExecutionVenue` trait + multi-location inventory
   ledger so hedges can execute on DeepBook, a swap router, a CEX, or another
   chain, and holdings can live off-Sui.
6. **Vol/pricing upgrades** — implied-vol calibration instead of realized-vol
   quoting, term structure, calibrated smile.
7. **PnL attribution + time series** — which is also exactly the data model
   the future dashboard reads.

Sections 2–3 are the detailed audit; section 4 is the target architecture with
the math; section 5 is a phased roadmap.

---

## 2. Where the bot is today

### 2.1 Four quoting/bidding surfaces, one brain

All four surfaces price through the same pure function
(`mm-bot/src/pricing.rs::price_rfq`) fed by the same Pyth spot cross
(`compute_spot_from_cache`) and realized-vol buffers:

| Surface | File | Direction / position created | Enabled (prod) |
|---|---|---|---|
| WS signed-quote RFQ (retail flow) | `main.rs` serve loop | Writer-MM ask: **short covered call / short CSP**. Trader-MM bid: **long call/put** (receives option coin) | yes (both roles) |
| DeepBook resting quotes on bucket pools | `deepbook.rs` | Asks sell its long call coins; bids buy more. **Call-only** (`is_put: false`, deepbook.rs:541) | yes, daily refresh, full-inventory listing |
| On-chain RFQ auction bidder (vault call slices) | `onchain_rfq.rs` | Wins → **long calls** | yes |
| On-chain put RFQ bidder | `onchain_put_rfq.rs` | Wins → **long puts** | yes (config shape shared) |
| On-chain proceeds-swap bidder | `onchain_swap.rs` | Sells underlying for the vault's settlement — a **spot trade** priced off the Pyth cross ± `bid_margin_bps` | yes |

Note the last row: the swap bidder is already, in embryo, a spot-execution
engine — it values settlement at the oracle cross and pays underlying for it
under a margin. It just isn't connected to any notion of *why* the bot would
want that inventory.

### 2.2 The pricing brain

`price_rfq` (mm-bot/src/pricing.rs:315) composes, per quote:

- Black-Scholes mid (`crates/pricing`), r = 0 by protocol convention —
  deliberately chosen so European formulas are exact for the protocol's
  American-exercisable options (pricing/lib.rs:20-27), with a put intrinsic
  floor (`per_unit_at`) since the on-chain puts are American.
- **Vol-space spreads**: ask at `σ × ask_vol_markup`, bid at
  `σ × bid_vol_markdown` — spread scales with warehoused vega. Premium-bps
  floors (`ask_markup_bps`/`bid_markdown_bps`) keep the book two-sided deep
  ITM where vega ≈ 0.
- **Last-look / TTL charge**: `mult · |Δ| · S · σ · √(ttl_years)` — prices the
  free option a 30s-valid signed quote hands the taker.
- **Fallback-vol penalty** while the vol buffer is cold, **size widening**
  proportional to clip notional, **max notional gate**, and a parametric
  **smile** `σ(K) = σ_atm(1 + skew·z + convexity·z²)` — shipped flat
  (uncalibrated) with per-symbol override hooks.

Sigma is **realized vol**: `max(RV_24h, RV_168h)` from 5-minute Pyth samples
(`resolve_sigma`, `RollingVolBuffer`), with a per-symbol config fallback.

This is a genuinely respectable quoting stack for its stage. Its weaknesses
(§3.6) are about *what* σ is, not how it's plumbed.

### 2.3 Market data

Single Pyth gateway (oracle-service) → WS fanout → push-fed `PriceCache` read
on the hot path with three quote gates: local observation age, Pyth publisher
lag, and Pyth confidence-interval width (`Staleness`). Good pick-off
protection at the *data* layer.

### 2.4 Inventory, collateral, funding

- Funds live in three-plus places already: the Sui **wallet** (auction escrow,
  DeepBook top-ups, swap funding), the MM's own **`mm_collateral`
  CollateralAccount** (backs signed quotes; routing is inside the signed
  payload), and the DeepBook **BalanceManager**. There is no unified ledger;
  each loop reads whichever balance it needs ad hoc.
- `LiquiditySource` (liquidity.rs) is a pluggable **funding** trait
  ("make sure I hold ≥ target of coin X"), default-implemented by the test
  faucet. It is explicitly the extension point for real funding — but it is
  *not* an execution abstraction: no price, no sizing against a market, no
  fill reporting, no venue choice.
- Writer-side inventory is auto-replenished by faucet mint on a threshold —
  the exact place a hedger belongs in production.

### 2.5 What's already good (keep it)

- **Pure, unit-tested decision functions** (`price_rfq`, `decide_bid`,
  `size_targets`) with IO pushed to the edges. The hedging engine should copy
  this shape.
- One pricing implementation across every venue — no drift between surfaces.
- Staleness/confidence gates; benign-race classification
  (`is_benign_bid_loss`) vs paging alerts per `.claude/tx-alerting.md`.
- Greeks, IV solver, assignment probabilities, smile, and the z-ladder strike
  grid already live in `crates/pricing` — the risk engine's math is ~done.
- Escrow accounting across concurrent auctions (`max_concurrent_escrow`).

---

## 3. The core gap: the bot prices risk but never holds it

### 3.1 No trade capture

Nothing in the service records a fill. When a signed WS quote is executed on
chain, the bot never learns; when the keeper settles a won auction, call coins
just appear in the wallet (the DeepBook loop notices only as "sweepable
inventory", deepbook.rs:369). There is no blotter, no persistence, no
reconciliation. **Every capability below depends on fixing this first.**

### 3.2 No position book, no greeks

`call_greeks`/`put_greeks` exist but are called only by api-service's
stateless `/options/metrics` endpoint. The bot never computes the delta of
anything it owns. It cannot answer "what is my net delta in TBTC right now?"

### 3.3 No hedging

No spot hedging loop exists at all. The only spot-trading code (the proceeds
swap bidder) trades *for edge against the oracle*, not to neutralize book
risk.

### 3.4 No PnL

No marks, no realized/unrealized split, no attribution, no success metrics.
The dashboard the team wants has nothing to read.

### 3.5 No lifecycle management of its own option inventory

The bot accumulates **long** calls/puts (auction wins, Trader-MM fills) and
**short** positions (Writer-MM `Position` objects), and then… nothing:

- No exercise logic anywhere in the service (`grep exercise` returns nothing).
  The options are American; a long ITM call the DeepBook ask never sold should
  be exercised (or at minimum exercised at expiry), else its intrinsic value
  evaporates at expiry. Today that value is silently lost.
- No redemption of writer `Position` objects post-expiry.
- The DeepBook quoter cancels near expiry (good, anti-sniping) but nothing
  disposes of what's left.

### 3.6 Smaller but real gaps and quirks

1. **Resting quotes go stale for up to a day.** Prod refresh cadence is
   86,400s with `order_lifetime_secs = 86_400`. The drift-based requote
   (`requote_drift_bps = 50`) is only *evaluated inside a cycle*, and cycles
   only run on the daily tick or on inventory arrival — so a 20% intraday
   move never triggers a requote. Resting *option* orders carry gamma; a
   day-stale ask after a rally is free money for a sniper. A professional
   quoter re-evaluates on a price trigger (subscribe to the price cache, fire
   a cycle when |mid drift| > threshold), not just on a clock.
2. **Realized vol is a floor estimator, not a market price of vol.**
   `max(RV_24h, RV_168h)` systematically undersells vol regimes the market
   prices in advance (events, weekends) and ignores the variance risk premium.
   `ask_vol_markup = 1.10` is a blunt bandage. There is also **no term
   structure** — a 2-hour option and a 30-day option quote the same σ.
3. **The smile is shipped flat** and there is no calibration loop, despite the
   IV solver existing.
4. **Oracle price ≠ executable price.** Everything is priced off Pyth. For
   quoting that's defensible; for *hedging* the fill price comes from a venue
   with its own depth/fees — a slippage model per venue is required.
5. **Covered-call collateral is delta the book must see.** Writing a covered
   call locks underlying in the bucket; economically the bot is long 1 spot +
   short 1 call (net Δ = 1 − N(d1) ≥ 0). If the position book naively counts
   only the short call it will double-hedge.
6. Cash-secured puts lock settlement (cash) — no spot delta from collateral,
   but the short put is Δ = 1 − N(d1) > 0 exposure the book must carry.

---

## 4. Target architecture

### 4.1 Module layout (new crates/modules, existing ones untouched)

```
services/mm-bot/src/
  pricing.rs            (exists — gains an inventory-skew input)
  deepbook.rs, onchain_*.rs, main.rs (exist — emit fills into `book`)
  book/                 position book: blotter, reconciler, persistence
  risk/                 greeks engine, limits, kill switch
  hedge/                hedger loop, band policy, hedge sizing
  venues/               ExecutionVenue trait + implementations
  ledger/               multi-location inventory (wallet / collateral /
                        BM / CEX / other-chain)
  pnl/                  marks, attribution, time series
```

The pure/IO split should mirror `price_rfq`/`decide_bid`: `risk::net_greeks`,
`hedge::decide_hedge`, `pnl::attribute` are pure functions over snapshots;
loops around them do IO.

### 4.2 Position book & trade capture

**Sources of truth.** All fills are observable on chain; the indexer already
decodes and stores the relevant events (`WriteExecuted` /
`CollateralizedWrite`, `RfqSettled`, swap settlement, DeepBook fills via
checkpoint stream). Two ingestion options:

- *Preferred:* subscribe/poll the indexer's GraphQL (`events(filter, …)`)
  filtered to the bot's own addresses (wallet, collateral account, BM,
  `token_recipient`) — reuses existing infra, survives bot downtime.
- *Fallback/cross-check:* periodic chain reconciliation — enumerate call/put
  coin balances, `Position` objects, CollateralAccount balances, BM balances,
  open auction escrows — and diff against the book. Run it on boot (cold-start
  recovery) and on a slow timer (drift detection). Any mismatch pages.

**The book** (Postgres via diesel, same stack as scheduler/indexer):

- `trades`: ts, venue (ws_rfq | deepbook | rfq_auction | put_auction | swap |
  hedge:*), instrument (bucket id or spot pair), side, qty, price, fees, tx
  digest (idempotency key).
- `positions`: current net per instrument, derived from trades and
  reconciliation; option positions carry (strike, strike_scale, expiry,
  is_put, direction).
- `inventory`: spot balances per (asset, location) — see §4.5.
- `greeks_snapshots`, `pnl_snapshots`: append-only time series (dashboard
  feed).

**Lifecycle tasks** (new, small, independent loops):
- exercise-if-ITM for long American options approaching expiry (net of fees
  and remaining time value — exercise when intrinsic > model value − ε, or
  unconditionally in the final window before `SETTLE_BUFFER_MS`);
- redeem writer `Position` objects after expiry;
- both alert with `alert_id = "tx-failed-mm-bot-…"` per tx-alerting.md.

### 4.3 Risk engine

Pure function: `net_greeks(book, spot, σ-surface, now) -> PerUnderlying<Greeks>`.

Sign conventions per flow (per unit of underlying):

| Position source | Delta | Gamma | Vega |
|---|---|---|---|
| Long call (auction win, Trader-MM fill) | +N(d1) | + | + |
| Written covered call (Writer-MM) = locked spot + short call | +1 − N(d1) | − | − |
| Long put (put auction win) | N(d1) − 1 | + | + |
| Written cash-secured put | 1 − N(d1)… **no**: short put = −(N(d1) − 1) = **1 − N(d1) − 1 + 1**; use `−put_delta` = N(−d1) shifted: short put Δ = −(N(d1)−1) | − | − |
| Spot inventory (any location) | +1 | 0 | 0 |
| Resting DeepBook order | 0 until fill; optionally probability-weighted | – | – |

(Implementation note: don't re-derive signs by hand as the table above shows
how easy it is to fumble — express *every* position as
`qty_signed × greeks(inputs)` with `qty_signed < 0` for written options, and
add `+1 per locked underlying unit` for covered-call collateral. Unit-test
against bump-and-reprice of the *portfolio* value, the same way
`crates/pricing` tests individual greeks.)

Outputs, per underlying and global: Δ (units and USD), Γ (USD per 1%²), vega
(USD per vol point), θ/day, plus assignment-prob-weighted expiry exposure.
Publish all of them as `metrics::gauge!` immediately — greeks telemetry is
valuable weeks before the hedger ships, and it exercises the whole book
pipeline.

Recompute triggers: on every fill, on a price-move threshold (reuse the
`PriceCache`), and on a slow timer (theta/time decay).

### 4.4 Hedging engine

**Delta (phase 1 — hedgeable with spot).** Per underlying:

```
Δ_book  = Σ qty_signed_i · delta_i  +  spot_inventory_units (all locations)
target  = 0 (configurable tilt)
error   = Δ_book − target
```

Rehedge policy: **band, not continuous**. Continuous rehedging bleeds fees and
churns on oracle noise; the standard cost-aware result (Whalley–Wilmott
asymptotics of the Hodges–Neuberger problem) gives a no-trade half-width

```
H ≈ ( (3/2) · ε · S · Γ_book² / λ )^{1/3}
```

with ε = proportional venue cost (DeepBook taker ≈ 10–12.5 bps, see
`tools/deepbook-pool-test/DEEPBOOK-FINDINGS.md`) and λ = risk aversion. Start
simpler and defensibly: a static band in *USD delta* (e.g. rehedge when
|Δ_USD| > X, trade back to Y < X, never to exactly zero) plus a per-hour trade
cap; graduate to the Γ-scaled band once fill data exists to calibrate λ.
Trigger the hedger on fills immediately (a fill is a known, discrete risk
jump) and on price drift otherwise.

Hedge orders go through the venue router (§4.5). Every hedge is a `trades`
row (`venue = hedge:<name>`), so PnL attribution can split option edge from
hedge slippage. Failures follow tx-alerting
(`alert_id = "tx-failed-mm-bot-hedge"`); repeated failure past a threshold
trips the kill switch (§4.7) because an unhedged book plus dead hedger is the
worst state to keep quoting in.

**Gamma / vega / theta (phase 2+).** These cannot be hedged with spot; be
explicit that the near-term strategy is **constrain and skew, not hedge**:

1. **Limits**: hard per-underlying caps on |Γ|, |vega|, θ (§4.7). Quoting
   declines (or one-sides) when a fill would breach them.
2. **Inventory-aware quoting** (the professional first line of defense):
   shift the quoted vol against the book — e.g.
   `σ_quote = σ_mid · (1 + κ · vega_book / vega_ref)` so a vega-long book
   bids vol lower and asks lower (sheds), a vega-short book the reverse; and
   shift the mid by an Avellaneda–Stoikov-style reservation adjustment
   `−q · λ · σ² · τ` in delta terms. Plumbing-wise this is one new input to
   `PricingConfig` (a per-underlying `InventorySkew { vol_shift, mid_shift }`
   snapshot the risk engine refreshes), keeping `price_rfq` pure.
3. **Offsetting optionality** (later): the bot already sits in the middle of
   two option markets — it can post DeepBook quotes *designed to shed* (e.g.
   list more of its long calls when vega-long) and bid auctions more/less
   aggressively as a function of book greeks. True cross-venue vega hedging
   (Deribit etc.) becomes possible once the CEX venue adapter exists, but it
   is not a prerequisite.

Theta is the payment received for warehousing gamma — monitor the θ/Γ ratio
per underlying rather than "hedging" theta.

### 4.5 Execution venue abstraction + multi-location inventory

Two distinct abstractions, both needed:

**(a) `ExecutionVenue`** — *how to trade spot*:

```rust
#[async_trait]
pub trait ExecutionVenue: Send + Sync {
    fn name(&self) -> &str;
    fn location(&self) -> Location;           // where the fill settles
    /// Executable-price estimate for `qty` of `pair`, fees + impact included.
    async fn quote(&self, pair: &SpotPair, side: Side, qty: u64)
        -> Result<VenueQuote>;                 // px, fee, max_qty, ttl
    /// Fire the order. Must be idempotent per `client_id`.
    async fn execute(&self, order: SpotOrder) -> Result<ExecutionReport>;
    /// Poll async fills (CEX / cross-chain venues fill later).
    async fn pending(&self) -> Result<Vec<ExecutionReport>>;
}
```

Implementations, in order of build-out:

1. **`SimulatedVenue`** (testnet, first): fills instantly at the Pyth cross ±
   configurable slippage bps, minting/burning test tokens via the existing
   faucet plumbing. This *is* the "artificial spot liquidity" — it makes the
   hedger testable end-to-end, deterministically, with zero market infra.
2. **`DeepBookSpotVenue`**: taker IOC orders on real underlying/settlement
   pools. `sui_tx::tx::deepbook` already has BM management and
   `top_of_book`; it needs a taker (`place_market_order` / IOC) builder —
   today it only places post-only maker quotes. For testnet realism, pair it
   with a trivial faucet-backed LP bot resting Pyth±spread on a
   TBTC/TUSDC pool.
3. **`SwapRouterVenue`**: Cetus/Turbos/Aftermath aggregator adapter
   (quote-then-swap PTB).
4. **`CexVenue` / `ExternalChainVenue`** (stubs now, per the requirement):
   the trait shape above already accommodates them — async fills via
   `pending()`, settlement into a non-Sui `Location`.

A thin **router** sits on top: given a hedge order, query `quote()` across
enabled venues, pick best all-in price subject to per-venue caps, split if
needed. V1 can be "priority list with fallback"; best-execution routing is a
later refinement.

**(b) `ledger::Location`** — *where assets live*:

```rust
pub enum Location {
    SuiWallet, SuiCollateralAccount, SuiBalanceManager,
    Cex { name: String }, ExternalChain { chain: String, account: String },
}
```

Delta is location-agnostic (CEX BTC hedges Sui TBTC exposure — modulo basis,
which gets a config haircut), but *usability* is not: quoting collateral must
be in the CollateralAccount, DeepBook bids in the BM, auction escrow in the
wallet. So the ledger tracks `(asset, location) -> balance` plus in-flight
transfers, and a **rebalancer** loop moves funds between locations against
per-location min/max targets — this generalizes today's ad-hoc
wallet→BM sweeps and faucet top-ups, and `LiquiditySource` becomes just the
external-funding edge of it. Cross-location transfers are first-class ledger
entries (bridges/withdrawals take time; the hedger must see in-flight delta).

### 4.6 Vol & pricing upgrades

1. **Quote implied vol, not realized vol.** Build a per-underlying σ-surface
   object the risk engine and all four quoting surfaces share:
   - anchor: RV blend as today (it's a fine prior),
   - calibration: back out IV from *observed trades and competing quotes*
     (`crates/pricing::implied_vol` exists; the bot sees every RFQ it loses
     and every auction clearing price — that's free calibration data worth
     persisting from day one),
   - external reference: a Deribit/CEX IV feed adapter for BTC-class assets,
   - explicit variance-risk-premium multiplier replacing the flat
     `ask_vol_markup` guess.
2. **Term structure**: σ(τ) interpolation (short-tenor RV vs long anchor)
   instead of one σ for all expiries.
3. **Smile calibration**: fit `skew`/`convexity` per underlying from the
   collected quote/trade prints; the parametric form and per-symbol plumbing
   already exist.
4. **Event awareness** (later): weekend/announcement vol adjustments.
5. Keep r = 0 until settlement carries a funded rate; if that changes, the
   American-put early-exercise premium needs real treatment (binomial or
   approximation) — the current intrinsic floor is only exact at r = 0.

### 4.7 Portfolio risk limits & kill switch

Today's only gates are per-quote (`max_quote_notional`) and per-loop escrow
caps. Add book-level, checked pre-quote and pre-bid:

- per-underlying and global caps: |Δ_USD|, |Γ_USD|, |vega_USD|, net short
  option notional, count of open expiries;
- a **soft mode** (widen spreads / one-side quotes to shed) between healthy
  and breach;
- a **kill switch** (breach, hedger dead, oracle stale beyond N minutes,
  drawdown > X): decline all RFQs, cancel all DeepBook orders (the cancel
  sweep exists — deepbook.rs shutdown path), stop bidding auctions, keep
  hedging allowed (hedging *reduces* risk). Manual re-arm.
- Reconciliation mismatch (book vs chain) above a tolerance ⇒ soft mode +
  page.

### 4.8 PnL & attribution (feeds the dashboard)

Mark the book each snapshot (model marks off the σ-surface; DeepBook mid as a
cross-check where a book exists). Decompose PnL between snapshots per
underlying:

```
ΔPnL ≈ spread capture (trade px vs model mid at fill)
     + delta PnL      (Δ · dS)         ← should be ~0 if hedged; residual = hedge quality
     + gamma PnL      (½ Γ · dS²)
     + vega PnL       (vega · dσ)
     + theta          (θ · dt)
     + hedge costs    (fees + slippage vs quote() estimate)
     + residual
```

Success metrics worth storing per fill: markouts (mid at +1m/+5m/+1h vs fill
price — the standard toxicity measure; tells you which flow/venue is informed),
RFQ win rate vs decline rate, auction win price vs model, hedge slippage
distribution. All of it lands in the `*_snapshots` tables → the dashboard is a
read-only client (api-service already demonstrates the pattern, and its
`/options/metrics` + `/dashboard/*` endpoints show where it would plug in).

### 4.9 Persistence & recovery

Postgres (diesel, same as scheduler/indexer). Cold start: load book from DB →
reconcile against chain (own coins, `Position` objects, CollateralAccount, BM,
open escrows) → resume. The bot must be crash-safe: every state transition is
derived from chain-observable facts, DB is a cache of derived state, and the
reconciler is the authority. (This is the same "chain is the source of truth"
stance the QuoteSigner bootstrap already takes — keep it.)

---

## 5. Phased roadmap

Ordering principle: **observability before automation** — greeks you can see
buy immediate safety; a hedger without a trustworthy book is dangerous.

**Phase 0 — See the book (foundation, no behavior change)**
- Fill ingestion from indexer + chain reconciler; `trades`/`positions` in
  Postgres; option-lifecycle tasks (exercise-if-ITM, redeem-after-expiry —
  these also stop actively losing money today, §3.5).
- Risk engine: `net_greeks` pure fn + gauges/time series. Persist observed
  competitor quotes/auction prints for later vol calibration.
- Fix the stale-resting-quote hole: price-drift-triggered DeepBook requote
  cycles (the drift knob exists; it just never fires between daily ticks).

**Phase 1 — Delta hedging on Sui**
- `ExecutionVenue` trait + `SimulatedVenue` (artificial testnet liquidity) +
  `DeepBookSpotVenue` taker support in `sui_tx::tx::deepbook`.
- `ledger` with Locations + the rebalancer generalizing today's sweeps.
- Hedger loop with static USD-delta band; fills recorded as trades;
  tx-alerting on failures. Verify on testnet against `SimulatedVenue`, then a
  faucet-backed DeepBook spot pool.

**Phase 2 — Risk-aware quoting**
- Portfolio limits, soft mode, kill switch.
- Inventory skew input to `PricingConfig` (vol shift + mid shift from book
  greeks) across all four surfaces.
- Γ-scaled hedge band (Whalley–Wilmott) once fee/fill data calibrates it.

**Phase 3 — Venue generality**
- `SwapRouterVenue`; router with best-quote selection; `CexVenue` /
  `ExternalChainVenue` stubs compiling against the trait; basis haircut
  config for off-Sui hedges; in-flight transfer tracking.

**Phase 4 — Vol done properly**
- σ-surface object: IV calibration from prints, term structure, smile fit,
  VRP multiplier; external IV reference adapter.
- Put quoting on DeepBook (today call-only) if put bucket pools exist.

**Phase 5 — PnL attribution + dashboard**
- Attribution engine + markouts; HTTP read surface (api-service pattern) for
  positions / hedges / greek history / returns — the dashboard reads this.

---

## 6. Appendix: key code references

- Pricing brain: `services/mm-bot/src/pricing.rs:315` (`price_rfq`),
  spreads/knobs at `pricing.rs:22-70`.
- Greeks/IV (ready to reuse): `crates/pricing/src/lib.rs:155`
  (`call_greeks`), `:321` (`put_greeks`), `:189` (`implied_vol`), `:111`
  (`assignment_prob`); smile `crates/pricing/src/smile.rs`; strike grid
  `crates/pricing/src/grid.rs`.
- Vol estimation: `crates/pyth-client` (`RollingVolBuffer`), sampler at
  `services/mm-bot/src/main.rs:1659`.
- Funding trait to generalize: `services/mm-bot/src/liquidity.rs:34`.
- Faucet auto-replenish (what the hedger replaces):
  `services/mm-bot/src/main.rs:1600`.
- DeepBook quoting loop: `services/mm-bot/src/deepbook.rs` (call-only:
  `:541`; drift knob evaluated only per-cycle: `:605-619`); PTB builders
  `crates/sui-tx/src/tx/deepbook.rs` (maker-only today).
- Auction bidders: `services/mm-bot/src/onchain_rfq.rs` (`decide_bid` at
  `:149`), `onchain_put_rfq.rs`, `onchain_swap.rs` (`max_underlying_bid` at
  `:84` — the embryonic spot engine).
- Fill-observable events: indexer `services/indexer/src/event_types.rs`;
  GraphQL read surface `services/indexer/src/graphql.rs`.
- Venue fee facts: `rust-backend/tools/deepbook-pool-test/DEEPBOOK-FINDINGS.md`.
- Alerting convention: `.claude/tx-alerting.md`.
