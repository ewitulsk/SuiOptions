# 06 — Backtesting Engine (`crates/vault-sim` + `tools/backtester`)

**Files**: new `rust-backend/crates/vault-sim/`, new `rust-backend/tools/backtester/`,
additions to `crates/pricing` (doc 05 §7)
**Depends on**: nothing on-chain — **start immediately**; its output picks the launch
parameters (delta target, fees, slicing, premium denomination, reserve floor).

## 1. The problem it solves

The product has no premium history: nobody has ever quoted these options. So the engine
must (a) replay history where proxy data exists, (b) generate synthetic futures where it
doesn't, and (c) make the premium assumption an explicit, swept *parameter* with the
honesty to report bands, not points.

## 2. Math the engine implements

### 2.1 Round payoff under the cursor model

The vault writes `Q` units at bucket creation ⇒ occupies `[0, Q)` — front of the FIFO queue.
With cursor `c` at expiry: exercised `e = min(c, Q)`, fraction `φ = e/Q`. Per-unit value at
expiry (settlement units), premium `p` per unit:

```
V = (1 − φ)·S_T + φ·K + p
```

**Front-of-queue bound.** Holders other than the vault's counterparties also exercise into
the same bucket, but exercises consume the queue from the front — the vault absorbs them
first. Hence vault `φ` ≥ bucket-average exercise fraction, with equality at φ = 1; and since
`φ ≤ 1`, the vault's worst case is `φ = 1{S_T > K}` — the textbook covered call
`min(S_T, K) + p`. Every behavioral under-exercise scenario only raises `V` (for `S_T > K`,
`∂V/∂φ = K − S_T < 0`). 

**Early exercise weakly helps the writer.** If exercised at `t < T` (spot `S_t > K`), the
vault's exercised units are worth `K` regardless of the path after `t`: identical to expiry
exercise if `S_T > K`, strictly better if the price falls back (`K > S_T`). The exerciser
forfeited remaining time value to the writer. ⇒ **rational-at-expiry full exercise is the
conservative anchor**; all other policies are upside. The engine still simulates them to
quantify the upside, not to defend the downside.

### 2.2 Round return (underlying-denominated)

With swap slippage `s`, premium conversion policy `π ∈ {at_roll, at_expiry, hold_usdc}`:

```
OTM:  R = p/S_x · (1 − s)                       S_x = S₀ or S_T per π
ITM:  R = (K + p)/S_T · (1 − s) − 1             (φ = 1 worst case; engine uses actual φ)
```

USD-denominated: `V_usd = (1−φ)·S_T + φ·K + p` vs HODL benchmark `S_T`.

### 2.3 Ledger math = contract math

The `ledger` module reimplements doc 03 §7.4 **in chain units with identical rounding**:
u128 PPS at `PPS_SCALE = 1e12`, floor on share mint and withdraw, fee formulas and the
profitable-round gate, queue ordering (withdrawals then deposits at `pps[r]`), the
receipt-round convention (`round r` converts at `pps[r−1]`… per doc 03's normative rule).
Pricing runs in f64; **accounting runs in integers**. This is deliberate: the backtester
doubles as the reference implementation the Move tests diff against.

## 3. Crate layout

```
crates/vault-sim/src/
├── lib.rs
├── engine.rs        // round loop orchestration
├── ledger.rs        // integer share/fee accounting (mirror of vault.move §7.4)
├── cursor.rs        // bucket/cursor mechanics incl. apply_strike round-half-up (mirror of bucket.move)
├── strategy.rs      // StrikeSelector: delta target + z-ladder snap (mirrors keeper strike.rs
│                    //   AND scheduler grid v2 — same ladder, same rounding, so the sim sees
│                    //   exactly the strikes production would create)
├── iv.rs            // IvProvider trait + impls
├── premium.rs       // PremiumModel trait + impls
├── sale.rs          // SaleMechanism trait + impls (incl. on-chain auction model)
├── exercise.rs      // ExercisePolicy trait + impls
├── paths.rs         // PathSource trait: historical replay + synthetic generators
├── swap.rs          // slippage model for USDC↔U conversions
├── metrics.rs       // per-round records → summary statistics
└── types.rs         // chain-unit newtypes (UnderlyingAmt, SettleAmt, Pps), round records

tools/backtester/src/
├── main.rs          // CLI: run scenario file(s), parallel sweep (rayon), emit reports
├── data.rs          // CSV/Parquet loaders: candles, DVOL, deribit chains
├── scenario.rs      // TOML scenario schema + cartesian sweep expansion
└── report.rs        // CSV per-round dump + JSON summary + markdown comparison table
```

## 4. Core traits

```rust
/// Daily (or finer) bars driving the simulation.
pub trait PathSource {
    fn next_path(&mut self) -> Option<Path>;     // replay yields 1; MC yields N
}
pub struct Path { pub bars: Vec<Bar> }           // Bar { ts_ms, open, high, low, close }

pub trait IvProvider {
    /// Annualized IV for (tenor, target delta) at time t along the path.
    fn iv(&self, ctx: &MarketCtx, tenor_years: f64, delta: f64) -> f64;
}
// impls: DeribitDvol { skew_adj }            — BTC/ETH: DVOL series + skew parameter
//        VrpTransfer { ref_iv, ref_rv }      — SUI: RV_sui × (IV_btc / RV_btc)
//        BetaScaled  { beta_window }         — SUI: (RV_sui/RV_btc) × IV_btc
//        RealizedOnly                        — floor scenario: IV = RV

pub trait PremiumModel {
    /// Mid premium per unit (settlement units) the market would pay.
    fn mid(&self, q: &QuoteCtx) -> f64;          // default: BS(spot,K,τ,r,iv)
}

pub trait SaleMechanism {
    /// Premium actually realized for a slice, given mid. Returns per-unit
    /// executed premium and filled amount (may be < slice for auction model).
    fn execute(&mut self, slice: u64, mid: f64, ctx: &QuoteCtx) -> Fill;
}
// impls:
//   RfqBatch   { haircut_bps }                       — fills at mid × (1 − h); models the
//                                                      signed-quote path and a competitive
//                                                      on-chain auction equally
//   OnchainAuction { n_bidders_dist, markdown_bps,   — doc 02 model: each bidder's max bid =
//                    reserve_bps, no_show_prob }       mid × (1 − markdown_i); clearing =
//                                                      2nd-highest max + increment, floored
//                                                      at reserve; 0 bidders ⇒ unsold slice
//   Pessimist  { fill_prob, haircut_bps }            — stress scenario

pub trait ExercisePolicy {
    /// Called each bar while the option lives; returns incremental exercised
    /// fraction of the bucket [0,1].
    fn step(&mut self, bar: &Bar, k: f64, t_left: f64) -> f64;
}
// impls: RationalExpiry                — φ_T = 1{S_T > K}        (conservative anchor)
//        EarlyIntrinsic { thresh_bps } — exercise all once S/K − 1 > thresh
//        Partial { frac }              — φ_T = frac × 1{S_T > K}
```

`StrikeSelector` is concrete (not a trait): z-ladder grid construction (doc 05 §4.2,
shared constants) + delta-target snap-up (doc 04 §3), with the delta target a scenario
parameter (sweep 0.05–0.30).

## 5. Data plan

| Dataset | Source | Span | File |
|---|---|---|---|
| BTC daily+hourly candles | Binance/Coinbase export | 2019→ | `data/btc_usd_{1d,1h}.csv` |
| ETH daily candles | same | 2019→ | validation only |
| SUI daily+hourly candles | Binance SUI/USDT | 2023-05→ | `data/sui_usd_{1d,1h}.csv` |
| BTC & ETH DVOL | Deribit public API `get_volatility_index_data` | 2021-03→ | `data/{btc,eth}_dvol_1d.csv` |
| Deribit weekly chain marks (optional, paid: Tardis/Amberdata) | — | 2021→ | premium-model validation |
| Ribbon T-ETH-C / T-WBTC-C weekly results | Dune / archived dashboards | 2021–2023 | engine validation target |

CSV schemas documented in `tools/backtester/data.rs`; a `fetch-data.sh` helper script per
source (candles via exchange REST, DVOL via Deribit JSON-RPC). Keep raw files out of git
(`data/` in `.gitignore`); commit only the tiny validation fixtures.

The `iv_ratio` used by the keeper (doc 04 §3) is calibrated here: median DVOL/RV₃₀ over the
sample, per regime. Report it in the calibration output.

## 6. Engine loop (per path)

```
for each round window [T_r, T_r + 7d):
  1. settle prior round: cursor → φ via ExercisePolicy accumulation; redeem;
     swap proceeds per π policy through swap.rs slippage
  2. ledger.finalize: fees (profitable gate), pps[r], withdrawals, deposits
     (deposit/withdraw flow itself is scenario-driven: constant TVL | growth curve |
      stress redemption schedule)
  3. strike: σ_iv = IvProvider.iv(…, 0.10); grid = z_ladder(spot, σ_grid, τ);
     K = snap_up(K*(σ_iv), grid)
  4. sell: for each slice in slicing schedule:
       mid = PremiumModel.mid(...); fill = SaleMechanism.execute(...)
       premium += fill;  unsold stays idle this round
  5. step bars daily/hourly until expiry, feeding ExercisePolicy
record RoundRecord { spot₀, S_T, K, σ_iv, premium, φ, fees, pps, unsold, … }
```

## 7. Scenario schema (TOML; cartesian sweep)

```toml
[scenario]
asset            = "SUI"               # SUI | BTC | ETH
paths            = "replay"            # replay | bootstrap | gbm_jump | garch
n_paths          = 2000                # MC only
rounds           = 156                 # 3y horizon for MC

[strategy]
delta_target     = [0.05, 0.10, 0.20, 0.30]
deploy_fraction  = [1.0]
slices           = [1, 4]
premium_policy   = ["at_roll", "at_expiry", "hold_usdc"]   # ← the empirical pick

[premium]
iv_provider      = ["vrp_transfer", "beta_scaled", "realized_only"]
skew_bps         = [-500, 0, 1000]
mechanism        = ["rfq_batch", "onchain_auction"]
haircut_bps      = [0, 500, 1000, 1500]
n_bidders_mean   = [1, 3, 6]           # auction model

[exercise]
policy           = ["rational", "early_500bps", "partial_60"]

[fees]
mgmt_bps_annual  = [0, 200]
perf_bps         = [0, 1000]

[swap]
slippage_bps     = [10, 50]
```

The sweep runner (rayon-parallel; each cell is independent and fast — integer ledger + a few
thousand BS evals) emits one summary row per cell.

## 8. Synthetic paths (`paths.rs`)

- **Block bootstrap** (headline for SUI): resample 10-day blocks of historical daily
  log-returns (preserves fat tails + vol clustering, assumption-free). Stationary bootstrap
  (geometric block lengths) to avoid seam artifacts.
- **GBM + Merton jumps**: μ, σ, jump intensity/size fit to history — parametric cross-check.
- **GARCH(1,1)**: vol-clustering cross-check; also supplies a per-bar σ series so the IV
  proxy can be path-consistent (IV_t = f(RV_t of the synthetic path), not of real history).
- Seeded (`rand_chacha`) for reproducibility; path seeds recorded in reports.

## 9. Outputs & validation

### 9.1 Metrics per scenario cell

APY (Ribbon 4-week formula **and** full-sample geometric, both denominations), premium yield
per round (p5/p50/p95), realized call-away frequency (target ≈ N(d2) ≈ 8–9% at 0.10Δ), max
drawdown, Sharpe/Sortino, 95% ES (USD), vs-HODL tracking + upside capture in bull windows
(flag 2023-Q4/2024-Q1-style segments explicitly), fee revenue, unsold-slice rate.
MC scenarios add the cross-path distribution of 1-year return and P(underperform HODL).

### 9.2 Property tests

Conservation: every settlement unit of premium/exercise proceeds and every underlying unit
traces through `cursor.rs` + `ledger.rs` with exact integer equality. Payoff identities:
RfqBatch with h=0, rational exercise, zero fees/slippage reproduces analytic covered-call
returns to rounding dust. PPS invariants from doc 03 §11 (5) and (6).

### 9.3 Cross-validation against the Move contracts

Golden files: a scripted localnet round (doc 07) dumps event-derived round records; the
backtester replays the same inputs; ledgers must match to the unit.

### 9.4 Engine validation milestones (gate the launch decision on these)

1. **BS sanity**: priced premiums vs `crates/pricing` goldens. ✔ trivial
2. **Ribbon replication**: engine in "Ribbon mode" (European, cash-settled, real DVOL,
   2/10 fees) on ETH/BTC 2021–2023 vs Ribbon's published weekly results — APY within
   ±2pts, drawdown shape and ITM-week count matching. This validates strike selection +
   accounting end-to-end against the only real-world track record available.
3. **Premium validation (BTC)**: modeled 10Δ weekly premium vs actual Deribit marks (paid
   data, or spot-check archived chains) — fit `skew_bps` and `haircut_bps` here, then
   transfer the fitted values to SUI scenarios.
4. **Forward shadow test** (post-RFQ-launch): paper-run the strategy against live on-chain
   auction results for ≥ 4 weeks; compare to engine prediction; recalibrate
   `n_bidders/markdown/haircut`. The keeper's per-round σ/K/premium logs (doc 04 §3) are
   the data source.

## 10. CLI UX

```
backtester run  --scenario scenarios/sui_launch.toml --out out/sui_launch/
backtester sweep --scenario scenarios/sweep.toml --out out/sweep/ --top 20 --rank apy_p5
backtester validate-ribbon --asset eth --out out/ribbon_eth/
backtester report --in out/sweep/ --format md       # the table that picks launch params
```

Per-run outputs: `rounds.csv` (every round record), `summary.json`, `config.toml` (frozen
resolved scenario), seed list. The `report` subcommand produces the comparison markdown for
the launch-parameter decision: delta target, fees, slices, premium policy, reserve floor.
