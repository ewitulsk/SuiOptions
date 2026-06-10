#!/usr/bin/env bash
# Fetches the backtester's data plan (doc 06 §5) into tools/backtester/data/:
#
#   data/btc_usd_1d.csv   Coinbase Exchange BTC-USD daily candles (2019→)
#   data/eth_usd_1d.csv   Coinbase Exchange ETH-USD daily candles (2019→)
#   data/sui_usd_1d.csv   Coinbase Exchange SUI-USD daily candles (2023-05→)
#   data/btc_dvol_1d.csv  Deribit BTC DVOL daily closes (2021-03→)
#   data/eth_dvol_1d.csv  Deribit ETH DVOL daily closes (2021-03→)
#
# Both sources are public, no keys. Candle schema: ts_ms,open,high,low,close.
# Vol schema: ts_ms,dvol (index points; 55.0 ⇒ 55%). Raw files stay out of
# git — see .gitignore.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p data

python3 fetch_data.py "$@"
