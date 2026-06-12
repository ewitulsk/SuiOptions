#!/usr/bin/env bash
# Pulls every dataset the backtester uses (doc 06 §5) — all free sources:
#
#   fetch_data.py            Coinbase daily candles (BTC 2015→, ETH 2016→,
#                            SUI 2023→) + hourly (DVOL era) + Deribit DVOL
#                            → data/ (gitignored, refresh anytime)
#   fetch_ribbon.py          Ribbon V2 vault round history from Ethereum
#                            logs via free public RPCs
#                            → datasets/ribbon_rounds.csv (committed)
#   fetch_chain_snapshots.py first-of-month Deribit option chains (mark
#                            price + IV) via Tardis's free keyless tier
#                            → datasets/chain_snapshots.csv (committed)
#
# The datasets/ outputs are static history and are committed so the
# calibration + Ribbon validation reproduce without network access.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p data datasets

python3 fetch_data.py "$@"
python3 fetch_chain_snapshots.py
python3 fetch_ribbon.py
