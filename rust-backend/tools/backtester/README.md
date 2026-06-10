# backtester

Offline backtesting engine for the covered-call vaults — the CLI over
`crates/vault-sim`. Design and methodology:
[`docs/vault-implementation-guide/06-vault-sim.md`](../../../docs/vault-implementation-guide/06-vault-sim.md).

```sh
# 1. Pull market data (Coinbase candles + Deribit DVOL; public APIs, no keys).
./fetch-data.sh            # everything; or ./fetch-data.sh sui

# 2. Calibrate the IV/RV ratio used by the vrp_transfer proxy (and the
#    keeper's iv_ratio config).
cargo run -p backtester -- calibrate \
    --candles data/btc_usd_1d.csv --dvol data/btc_dvol_1d.csv

# 3. Sweep launch parameters.
cargo run --release -p backtester -- sweep \
    --scenario scenarios/sui_launch.toml --out out/sui_launch/ \
    --top 20 --rank apy_p5

# Single-cell run with full per-round CSV dumps:
cargo run -p backtester -- run --scenario scenarios/btc_validation.toml --out out/btc/

# Re-render a sweep's table with a different ranking:
cargo run -p backtester -- report --in out/sui_launch/ --rank es95
```

Outputs per run directory: `summary.json` (one row per scenario cell),
`report.md` (ranked comparison table), `config.toml` (frozen scenario),
`seeds.json` (path seeds for reproduction), and `rounds_cell*.csv`
per-round dumps for `run`.

`data/` and `out/` are gitignored; `fixtures/` holds small committed
samples of the real files for tests and examples. `datasets/` holds
committed static history scraped from free sources: `ribbon_rounds.csv`
(Ribbon V2 round-by-round history read straight from Ethereum logs) and
`chain_snapshots.csv` (first-of-month Deribit chains via Tardis's free
tier).

## Validation & calibration (all free data)

```sh
# Milestone 2: replay Ribbon's track record against our model.
cargo run -p backtester -- validate-ribbon --vault T-ETH-C     --candles data/eth_usd_1d.csv --dvol data/eth_dvol_1d.csv     --out out/ribbon_eth --skew-bps=-600 --haircut-bps 2500

# Milestone 3: where the 10Δ weekly wing prices vs the DVOL index.
cargo run -p backtester -- calibrate-skew --dvol data/btc_dvol_1d.csv

# IV/RV ratio for the vrp_transfer proxy.
cargo run -p backtester -- calibrate --candles data/btc_usd_1d.csv     --dvol data/btc_dvol_1d.csv
```

Measured on the scraped data (June 2026):

| Parameter | Measurement | Scenario value |
|---|---|---|
| `vrp_ratio` | BTC DVOL/RV₃₀ median 1.19 (ETH 1.08) | 1.19 |
| `skew_bps` | 10Δ-weekly IV / DVOL: p25/med/p75 ≈ 0.89/0.94/1.00 | [-1100, -600, 0] |
| `haircut_bps` | Ribbon Gnosis auctions cleared 17–35% under the wing mark | [1500, 2500, 3500] |

With (skew −600, haircut 2500), the modeled strategy reproduces Ribbon's
realized track on the matched auction rounds within the ±2pt milestone
gate: T-ETH-C −1.9pts (ITM weeks 1/41 vs 1/41), T-WBTC-C +0.4pts.
