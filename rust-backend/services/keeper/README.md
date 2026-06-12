# vault-keeper — implementation guide

> Status: **implemented** (ticket D1). The crate in this directory
> follows the spec below; deviations from the original sketch are noted
> inline (planner returns one action per tick and signals
> `SelectBucketNeeded` for the tick loop to resolve; slicing is
> clock-derived rather than counted; metrics are structured log lines —
> the per-round σ/K*/strike/delta "strike pick" line is the calibration
> trail — with Prometheus counters still TODO). Spec originally derived
> from
> [`docs/vault-implementation-guide/04-vault-keeper.md`](../../../docs/vault-implementation-guide/04-vault-keeper.md),
> updated to match what shipped on-chain (PR #148). Remaining: the §12
> localnet e2e (ticket E1).

## 1. Trust model (read this first)

The keeper holds **only a gas wallet**. Every action it submits is a
public crank that `vault.move` validates on-chain:

- which bucket may be selected is bounded by the Pyth strike band and
  expiry-lead window (`vault::select_bucket`),
- the auction reserve floor is derived from Pyth inside
  `vault::open_rfq`,
- the proceeds-swap price is Pyth-bounded inside
  `vault::swap_proceeds`,
- everything else (`crank_redeem`, `settle_rfq`, `finalize_round`) has
  no degrees of freedom at all.

A malicious keeper cannot sell below the on-chain floors, but the floors
are not tight: the realistic worst case is picking the **lowest in-band
strike** (the +3% band edge instead of the ~0.10Δ target) **and** timing
the auction so only a colluding bidder shows up, who then pays exactly
the reserve. The leak per round is bounded by
`fair_premium(K_band_edge) − reserve_premium` on one slice — roughly
2–3% of the sliced notional at a 10 bps reserve. Mitigations: set
`min_reserve_premium_bps` to sit just under the strategy's expected
clearing premium — **30–50 bps at the Δ = 0.20 launch target** (guide
doc 08; note a 0.10Δ target snaps to strikes worth only ~10–15 bps, so
a high reserve and a low delta target cannot be combined) — and alert
when a round's clearing premium lands below the model price (the
per-round σ/K/premium metrics in §11 exist for exactly this). A lazy keeper's worst case is a delayed
round. N keepers can run concurrently and merely waste gas racing — lost
races abort with clear error codes. Anyone can run this binary; the team
runs ≥ 2 instances on independent infra.

## 2. Crate layout

Mirror the option-scheduler's structure (boot → tick loop →
classify/submit) — it solves the same problem:

```
services/keeper/
├── src/
│   ├── main.rs       # config, boot checks, tick loop (tick_secs ≈ 15)
│   ├── config.rs     # endpoints, pyth handles, strategy defaults
│   ├── discovery.rs  # vault auto-discovery + PriceInfoObject lookup
│   ├── state.rs      # VaultView: fetch + decode chain objects via RPC
│   ├── planner.rs    # PURE: (VaultView, now) → Vec<Action>
│   ├── strike.rs     # PURE: delta-target bucket choice (§5)
│   ├── slicing.rs    # PURE: slice schedule (§6)
│   ├── submit.rs     # Action → PTB via sui-tx builders; error classes
│   └── pyth.rs       # Hermes VAA → update_price_feeds PTB prefix (§7)
└── Cargo.toml        # sui-tx, pricing, pyth-client, runtime-config,
                      # token-info-client, api-service-client
```

The **planner is a pure function** — that is the testing surface.
Everything async (RPC reads, Hermes fetches, submission) stays outside
it.

## 3. State reading (`state.rs`)

Build a `VaultView` per configured vault each tick, via
`sui_sdk` object reads (the vault exposes every field the planner
needs as a public getter — see `contracts/sources/vault.move` §getters):

```rust
struct VaultView {
    round: u64,
    settling: bool,            // is_settling()
    current_bucket: Option<ObjectID>,
    current_expiry_ms: u64,    // 0 ⇒ no bucket was selected this round
    selling_ends_ms: u64,
    open_rfqs: u64,
    pending_positions: u64,    // positions_tail − positions_head
    deployable: u64,
    proceeds_settlement: u64,
    // plus, fetched alongside:
    live_buckets: Vec<BucketView>,   // from api-service /buckets (pair-filtered)
    open_auctions: Vec<RfqView>,     // vault-coupled RfqAuction objects
}
```

Decode the raw Move structs with BCS via `sui_sdk`'s
`get_object_with_options(..., bcs)` or read fields through the
`SuiParsedData` JSON — either is fine; pick one and golden-test it
against a localnet object. Auction discovery: subscribe to `RfqCreated`
events filtered by `origin == vault_id` (indexer GraphQL once C3 lands;
poll `api-service /rfqs?status=open` as fallback), or track the IDs
returned by our own `open_rfq` submissions and recover unknown ones from
events on restart.

## 4. The planner (`planner.rs`)

```rust
enum Action {
    CrankRedeem   { vault, bucket },             // batch ≤ K per PTB
    SettleRfq     { vault, rfq, bucket },
    SettleRfqExpired { vault, rfq, bucket },     // bucket died mid-auction
    SwapProceeds  { vault, pool, max_settlement_in }, // see §8
    FinalizeRound { vault },
    SelectBucket  { vault, bucket },             // chosen by strike.rs
    OpenRfq       { vault, bucket, slice_amount },
}
```

Tick logic, matching the as-built phase machine (note: a round that
never selected a bucket has `current_expiry_ms == 0` and is finalizable
immediately — `vault::maybe_enter_settling` flips the phase on the way
in, the keeper doesn't need a separate transition action):

```
if settling || now ≥ current_expiry_ms (incl. the ==0 idle case):
    if pending_positions > 0            → CrankRedeem
    elif open_rfqs > 0:
        for each tracked auction:
            bucket expired/invalidated  → SettleRfqExpired
            deadline passed             → SettleRfq
        (auctions can't be live here — create() enforced
         deadline + buffer ≤ expiry — but handle it anyway)
    elif proceeds_settlement > 0        → SwapProceeds       (§8)
    else                                → FinalizeRound
else (active, pre-expiry):
    if current_bucket.is_none()         → SelectBucket(strike.rs pick)
    elif now < selling_ends_ms          → OpenRfq per slicing.rs
    for each auction past its deadline  → SettleRfq
```

Stateless by design: the planner recomputes "what should exist by now"
from chain state alone, so keeper restarts and keeper races are
harmless.

## 5. Strike selection (`strike.rs`)

The contract enforces the band; the keeper picks the point in it.
All math already exists:

```rust
let sigma_iv = realized_vol * cfg.iv_ratio;          // §9 vol source
let k_star = pricing::strike_for_delta(spot, sigma_iv, tau, 0.0, 0.10);
// candidates: live buckets of the right pair whose expiry fits the
// vault's lead window; pick the SMALLEST strike ≥ k_star (snap up ⇒
// delta ≤ target ⇒ conservative). None ≥ k_star? take the highest
// in-band strike and log GridCoverageMiss (feeds the scheduler-grid
// alert, doc 05 §4.4).
```

This must stay behaviorally identical to
`vault_sim::strategy::StrikeSelector` (same `pricing::grid` ladder, same
snap rule) — that equivalence is what made the Ribbon validation
transferable. Add a test that runs both on shared vectors. Log
`(σ, K*, snapped strike, model delta)` with every `SelectBucket`: those
logs are the milestone-4 forward-shadow dataset
([06-vault-sim.md §9.4](../../../docs/vault-implementation-guide/06-vault-sim.md)).

## 6. Slicing (`slicing.rs`)

```toml
[slicing]
slices = 4            # RFQ slices per round
stagger_minutes = 90  # slice i opens at selling_start + i × stagger
retry_unsold = true   # re-open a failed slice once, same reserve
```

Pure schedule: from `selling_ends_ms`, `open_rfqs`, and remaining
`deployable`, compute "slices that should be open by now but aren't".
Slice size = `deployable / slices_remaining`, clamped to the vault's
`max_slice_amount`. The backtester swept slices ∈ {1, 4} — revisit the
default against `out/sui_launch` before launch.

## 7. Pyth price updates (`pyth.rs`) — the one new technical piece

`select_bucket`, `open_rfq`, `swap_proceeds`, and `finalize_round`
take two `&PriceInfoObject`s and enforce `max_price_age_secs` (config:
60s). The keeper must prepend a price update **in the same PTB**:

1. Fetch the latest VAA for both feeds from Hermes
   (`pyth-client::http` already wraps the REST endpoint; the update
   payload is `/v2/updates/price/latest?ids[]=…&encoding=base64`).
2. PTB prefix, mirroring the standard Pyth Sui flow:
   `wormhole::vaa::parse_and_verify(wormhole_state, vaa_bytes, clock)`
   → `pyth::pyth::create_price_infos_hot_potato(pyth_state, vec<VAA>, clock)`
   → `pyth::pyth::update_single_price_feed(pyth_state, potato,
   price_info_object, fee_coin, clock)` per feed (fee: split a few MIST
   from gas) → destroy the hot potato.
3. Then the vault crank call, reusing the same `PriceInfoObject` args
   (`sui_tx::tx::vault::PriceInfoRefs`).

Add this as `sui_tx::tx::pyth_update::prepend(pt, …)` so the mm-bot can
reuse it later. Object IDs needed in config: `pyth_state_id`,
`wormhole_state_id`, and the two `price_info_object_id`s per vault
(discoverable once from the feed ids via Pyth's state table; pin them in
config like the scheduler pins feeds).

## 8. Proceeds conversion (`SwapProceeds`)

`vault::swap_proceeds` is a real DeepBook v3 market order: the vault
sells its settlement proceeds for underlying against the **config-pinned
`Pool<Underlying, Settlement>`** (e.g. the canonical SUI/USDC pool — not
the call-coin pools), and the executed price must clear the Pyth cross
less `max_swap_slippage_bps` or the call aborts. Partial fills at a fair
price succeed (the bound applies to the executed portion); an empty book
aborts with `vault_proceeds_unswapped` — retry next tick. Because
`finalize_round` refuses unswapped proceeds, this action gates round
turnover.

The keeper supplies the **DEEP fee budget**: pass an owned `Coin<DEEP>`
(builder arg `deep_funding`; the unused remainder comes straight back),
or `None` for a zero coin on whitelisted pools. Keep a small DEEP
balance funded and alert on `deep_balance_low`. The pool object id comes
from config and must match the vault's pinned `deepbook_pool_id`
(`vault_wrong_pool` otherwise).

## 9. Vol source for `iv_ratio`

σ = 30-day realized vol from Pyth Benchmarks daily closes — **reuse
`option_scheduler::sigma::realized_sigma_from_benchmarks`** (move it to
`pyth-client` if the dependency feels wrong) — times the configured
`iv_ratio`. Calibrated values from the backtester (June 2026): BTC
DVOL/RV₃₀ median **1.19**, ETH **1.08**; default `iv_ratio = 1.15`
sits between. Static fallback per asset for benchmark outages, same
pattern as the scheduler's `sigma_fallback`.

## 10. Submission & error classes (`submit.rs`)

Copy `option-scheduler/src/roller.rs::classify_error`'s shape:

| Class | Meaning | Examples | Response |
|---|---|---|---|
| `Benign` | another keeper won the race, or state moved | aborts 35 `vault_wrong_phase`, 36/37 bucket sel., 30 `rfq_not_closed`, 39/40 ordering | drop, next tick |
| `Retry` | transient | RPC timeout, gas-object contention, Hermes 5xx | backoff, retry |
| `Fatal` | config bug | type-tag parse, feed mismatch (49), unknown object | alert + halt that vault |

Gas: one owned gas coin per keeper instance; never share a wallet
between instances (object contention).

## 11. Config sketch

Vaults are **discovered, not configured** (`src/discovery.rs`): the
tick loop reads the indexer's `vaults` view (fed by `VaultCreated`),
takes the pinned feed ids + decimals from each vault object, and
resolves the two `PriceInfoObject`s through the Pyth state's
`b"price_info"` table — the same lookup pyth-sui-js's
`getPriceFeedObjectId` does. A vault created on chain is picked up on
the next tick; with none on chain, the keeper idles with `/health` up.
The pinned DeepBook pool comes from the vault's own config; the DEEP
coin type from token-info. What remains in the TOML:

```toml
indexer_graphql_url = "http://indexer:9002/graphql"
tick_secs = 15
health_addr = "0.0.0.0:8086"

[pyth]
# Hermes must serve the SAME feed set the network's PriceInfoObjects
# are keyed by: Sui testnet = hermes-BETA, mainnet = stable. Benchmarks
# is stable-only, so on testnet sigma_fallback is load-bearing.
hermes_url = "https://hermes-beta.pyth.network"
benchmarks_url = "https://benchmarks.pyth.network"
pyth_package_id     = "0x…"   # latest (upgraded) package
wormhole_package_id = "0x…"
pyth_state_id       = "0x…"
wormhole_state_id   = "0x…"

[vault_defaults]                # strategy knobs, applied to every vault
iv_ratio = 1.15
target_delta = 0.20             # launch memo (guide doc 08)
sigma_fallback = 0.85
vol_window_days = 30
# deep_funding_coin = "0x…"     # keeper-owned Coin<DEEP> for swap fees
# deep_fee_per_swap = 1000000
[vault_defaults.slicing]
slices = 4
stagger_minutes = 90
```

Plus the standard `--dry-run` flag (full planning, log intents, submit
nothing) and Prometheus metrics: actions submitted/won/lost-race, redeem
backlog, time-to-finalize per round, grid-coverage misses, per-round
σ/K/premium (the calibration trail).

## 12. Tests

- **Planner tables** (pure): every phase/state corner — mid-settling
  restart, expired-but-unredeemed, unsold slices at window end,
  zero-deposit round, stale-view races, idle round
  (`current_expiry_ms == 0`), auction past deadline during Active.
- **Strike goldens**: (S, σ, τ) → K* vectors shared with
  `vault_sim::strategy`; snap-up; band-edge fallback.
- **Slicing**: schedule recomputation idempotence under restarts.
- **E2E (extends `rust-backend/tests`)**: localnet — scheduler creates a
  z-ladder family → users deposit → keeper (real binary) runs genesis
  finalize → select → 2 slices → scripted bidder (incl. a snipe; assert
  the deadline extension) → settle → warp past expiry → exercise 40% →
  redeem/fill/finalize → assert PPS, treasury fees, receipt payouts —
  and feed the same inputs to the backtester for the golden-file ledger
  diff ([06 §9.3](../../../docs/vault-implementation-guide/06-vault-sim.md)).

## 13. Build order

1. `config.rs` + `state.rs` (read a localnet vault end to end)
2. `planner.rs` + tests (pure, fastest feedback)
3. `pyth.rs` (the Hermes→PTB prefix) + one oracle-gated crank on testnet
4. `strike.rs` / `slicing.rs` + goldens
5. `submit.rs` classification + metrics + `--dry-run`
6. the §12 e2e

Already done, do not rebuild: the PTB builders
(`crates/sui-tx/src/tx/vault.rs` — `crank_redeem`, `select_bucket`,
`open_rfq`, `settle_rfq`, `settle_rfq_expired`, `swap_proceeds`,
`finalize_round`, plus `PriceInfoRefs`/`VaultRefs`), the strike math
(`pricing::{strike_for_delta, grid}`), and the vol fetch
(`option_scheduler::sigma`).
