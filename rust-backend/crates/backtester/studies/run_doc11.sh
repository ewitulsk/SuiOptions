#!/usr/bin/env bash
# The doc 11 study (PR O, SO-457): every stage writes under $OUT and
# `desk-backtester report` assembles results.json + report.md from it.
#
#   STORE_URL=file:///path/to/lake-mirror studies/run_doc11.sh /path/to/out [--open-holdout]
#
# Runtime on an 8-core laptop: doc 07 sweep ~5 min, SUI walk-forward
# ~10 min, BTC walk-forward ~25 min, stress ~5 min, grid ~15 min,
# capacity ~30 min. Expensive stages use the half-year SUI window on
# purpose (doc 11 §1 says so).
set -euo pipefail
OUT=${1:?out dir}
shift || true
HOLDOUT=${1:-}
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-desk-backtester}
SC=$HERE/../scenarios

# 1. Doc 07 §5 / doc 10 §2 reproduction: the doc 07 assumption (no
#    margin model) and the same desk under the Bluefin margin rules.
$BIN sweep --scenario "$SC/sui_doc07_calls.toml" --out "$OUT/doc07" --bands 1.5,3,5,10,20,30 --set margin.enabled=false
$BIN sweep --scenario "$SC/sui_doc07_calls.toml" --out "$OUT/doc07-margin" --bands 5,20,30

# 2. Estimator walk-forward, SUI and BTC (holdout sealed unless asked).
$BIN walkforward --config "$HERE/wf_sui_estimator.toml" --out "$OUT/walkforward-sui" $HOLDOUT
$BIN walkforward --config "$HERE/wf_btc_estimator.toml" --out "$OUT/walkforward-btc" $HOLDOUT

# 3. Stress suite on the half-year SUI window, 2025-10-10 in-sample.
$BIN stress --scenario "$SC/sui_mixed_halfyear.toml" --out "$OUT/stress" --at 2025-10-10

# 4. Break-even surface (central and conservative execution, three mixes).
$BIN grid --config "$HERE/grid_sui_halfyear.toml" --out "$OUT/grid-sui"

# 5. One small capacity frontier: two volumes, balanced, two seeds.
$BIN capacity --scenario "$SC/sui_mixed_halfyear.toml" --out "$OUT/capacity" --volumes 25000,100000 --mixes balanced --seeds 2

# 6. Assemble.
$BIN report --dir "$OUT" --out "$OUT/results"
