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
samples of the real files for tests and examples.
