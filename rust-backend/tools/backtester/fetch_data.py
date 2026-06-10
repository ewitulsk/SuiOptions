#!/usr/bin/env python3
"""Data fetcher for the vault backtester (doc 06 §5). Stdlib only.

Candles come from the Coinbase Exchange public API (300 bars per request,
paginated); the vol index from Deribit's public ``get_volatility_index_data``
JSON-RPC (DVOL, daily resolution). Output schemas match
``tools/backtester/src/data.rs``.
"""

import csv
import json
import sys
import time
import urllib.parse
import urllib.request
from datetime import datetime, timedelta, timezone

DATA_DIR = "data"
DAY_MS = 86_400_000

CANDLES = [
    # (coinbase product, output file, first day)
    ("BTC-USD", "btc_usd_1d.csv", "2019-01-01"),
    ("ETH-USD", "eth_usd_1d.csv", "2019-01-01"),
    ("SUI-USD", "sui_usd_1d.csv", "2023-05-18"),
]
DVOL = [
    # (deribit currency, output file, first day)
    ("BTC", "btc_dvol_1d.csv", "2021-03-24"),
    ("ETH", "eth_dvol_1d.csv", "2021-03-24"),
]


def get_json(url: str):
    req = urllib.request.Request(url, headers={"User-Agent": "suioptions-backtester"})
    for attempt in range(5):
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                return json.load(resp)
        except Exception as e:  # noqa: BLE001 — retry whatever the transport throws
            if attempt == 4:
                raise
            wait = 2**attempt
            print(f"  retry {url.split('?')[0]} after {e} ({wait}s)", file=sys.stderr)
            time.sleep(wait)


def fetch_candles(product: str, out_name: str, start_day: str) -> None:
    start = datetime.fromisoformat(start_day).replace(tzinfo=timezone.utc)
    end_of_data = datetime.now(timezone.utc).replace(
        hour=0, minute=0, second=0, microsecond=0
    )
    rows = {}
    cur = start
    while cur < end_of_data:
        window_end = min(cur + timedelta(days=299), end_of_data)
        qs = urllib.parse.urlencode(
            {
                "granularity": 86400,
                "start": cur.strftime("%Y-%m-%dT%H:%M:%SZ"),
                "end": window_end.strftime("%Y-%m-%dT%H:%M:%SZ"),
            }
        )
        url = f"https://api.exchange.coinbase.com/products/{product}/candles?{qs}"
        # Rows are [t_secs, low, high, open, close, volume], newest first.
        for t, low, high, open_, close, _vol in get_json(url):
            rows[int(t) * 1000] = (open_, high, low, close)
        cur = window_end + timedelta(days=1)
        time.sleep(0.25)  # public rate limit headroom

    path = f"{DATA_DIR}/{out_name}"
    with open(path, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["ts_ms", "open", "high", "low", "close"])
        for ts in sorted(rows):
            o, h, l, c = rows[ts]
            w.writerow([ts, o, h, l, c])
    print(f"{path}: {len(rows)} bars ({product})")


def fetch_dvol(currency: str, out_name: str, start_day: str) -> None:
    start_ms = int(
        datetime.fromisoformat(start_day).replace(tzinfo=timezone.utc).timestamp() * 1000
    )
    end_ms = int(time.time() * 1000)
    rows = {}
    cur = start_ms
    while cur < end_ms:
        # Deribit caps the response length; ~1000 daily points per request.
        window_end = min(cur + 900 * DAY_MS, end_ms)
        qs = urllib.parse.urlencode(
            {
                "currency": currency,
                "start_timestamp": cur,
                "end_timestamp": window_end,
                "resolution": 86400,
            }
        )
        url = f"https://www.deribit.com/api/v2/public/get_volatility_index_data?{qs}"
        data = get_json(url)["result"]["data"]
        # Rows are [ts_ms, open, high, low, close].
        for ts, _o, _h, _l, close in data:
            rows[int(ts)] = close
        cur = window_end + DAY_MS
        time.sleep(0.25)

    path = f"{DATA_DIR}/{out_name}"
    with open(path, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["ts_ms", "dvol"])
        for ts in sorted(rows):
            w.writerow([ts, rows[ts]])
    print(f"{path}: {len(rows)} points (DVOL {currency})")


def main() -> None:
    only = sys.argv[1] if len(sys.argv) > 1 else None
    for product, out, start in CANDLES:
        if only and only.lower() not in product.lower():
            continue
        fetch_candles(product, out, start)
    for currency, out, start in DVOL:
        if only and only.lower() not in currency.lower():
            continue
        fetch_dvol(currency, out, start)


if __name__ == "__main__":
    main()
