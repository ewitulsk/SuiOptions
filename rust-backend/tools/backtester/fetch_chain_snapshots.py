#!/usr/bin/env python3
"""Free Deribit option-chain snapshots via Tardis's keyless tier.

Tardis serves the FIRST DAY OF EACH MONTH of historical data for free, no
API key (their machine-readable replay endpoint). One snapshot per month of
the full Deribit chain — mark price AND mark IV per instrument plus the
spot index — is exactly what the premium calibration needs (doc 06 §9.4
milestone 3): how the ~10Δ weekly wing actually priced vs the ATM index,
month after month, back to 2019.

Output ``datasets/chain_snapshots.csv``: snapshot_date, currency, spot,
instrument, expiry_date, days_to_expiry, strike, mark_price (in the asset,
Deribit convention), iv. Filtered to calls expiring within 35 days at
0.8–2.5× spot — the region our vault trades. Resumable: months already in
the file are skipped.
"""

import csv
import gzip
import json
import sys
import time
import urllib.parse
import urllib.request
from datetime import date
from pathlib import Path

CURRENCIES = ["BTC", "ETH"]
START = date(2019, 4, 1)  # Deribit data on Tardis starts 2019-03-30
OUT = Path(__file__).parent / "datasets" / "chain_snapshots.csv"

FIELDS = [
    "snapshot_date",
    "currency",
    "spot",
    "instrument",
    "expiry_date",
    "days_to_expiry",
    "strike",
    "mark_price",
    "iv",
]

MONTHS = {
    "JAN": 1, "FEB": 2, "MAR": 3, "APR": 4, "MAY": 5, "JUN": 6,
    "JUL": 7, "AUG": 8, "SEP": 9, "OCT": 10, "NOV": 11, "DEC": 12,
}


def parse_instrument(name):
    """BTC-4MAR22-25000-C → (currency, expiry date, strike, is_call)."""
    parts = name.split("-")
    if len(parts) != 4:
        return None
    cur, exp, strike, kind = parts
    try:
        day = int(exp[:-5])
        month = MONTHS[exp[-5:-2]]
        year = 2000 + int(exp[-2:])
        return cur, date(year, month, day), float(strike), kind == "C"
    except (ValueError, KeyError):
        return None


def first_of_months(start):
    today = date.today()
    d = start
    while d < today:
        yield d
        d = date(d.year + (d.month == 12), d.month % 12 + 1, 1)


def snapshot_month(day, currency):
    """Stream the replay feed until the chain stops growing; return rows."""
    filters = json.dumps(
        [
            {"channel": "markprice.options"},
            {"channel": "deribit_price_index", "symbols": [f"{currency.lower()}_usd"]},
        ]
    )
    url = (
        "https://api.tardis.dev/v1/data-feeds/deribit?"
        + urllib.parse.urlencode({"from": day.isoformat(), "filters": filters})
    )
    # The endpoint refuses identity encoding (406) — gzip is mandatory.
    req = urllib.request.Request(
        url,
        headers={"User-Agent": "suioptions-backtester", "Accept-Encoding": "gzip"},
    )

    spot = None
    chain = {}
    lines = 0
    stale = 0
    with urllib.request.urlopen(req, timeout=120) as resp:
        for raw in gzip.GzipFile(fileobj=resp):
            lines += 1
            if lines > 12_000:  # safety stop; full chain arrives well before
                break
            try:
                payload = json.loads(raw.split(b" ", 1)[1])
            except (IndexError, json.JSONDecodeError):
                continue
            p = payload.get("params", {})
            ch = p.get("channel", "")
            if ch.startswith("deribit_price_index") and spot is None:
                spot = p["data"]["price"]
            elif ch.startswith("markprice.options"):
                before = len(chain)
                for d in p.get("data", []):
                    parsed = parse_instrument(d["instrument_name"])
                    if parsed and parsed[0] == currency and parsed[3]:
                        chain[d["instrument_name"]] = (parsed[1], parsed[2],
                                                       d["mark_price"], d["iv"])
                # The feed republishes the whole chain every minute; once a
                # full sweep adds nothing new we have the snapshot.
                stale = stale + 1 if len(chain) == before else 0
                if stale > 400 and spot is not None and chain:
                    break

    if spot is None or not chain:
        return []
    rows = []
    for name, (expiry, strike, mark, iv) in sorted(chain.items()):
        dte = (expiry - day).days
        if 0 < dte <= 35 and 0.8 * spot <= strike <= 2.5 * spot:
            rows.append(
                {
                    "snapshot_date": day.isoformat(),
                    "currency": currency,
                    "spot": spot,
                    "instrument": name,
                    "expiry_date": expiry.isoformat(),
                    "days_to_expiry": dte,
                    "strike": strike,
                    "mark_price": mark,
                    "iv": iv,
                }
            )
    return rows


def main():
    OUT.parent.mkdir(exist_ok=True)
    done = set()
    if OUT.exists():
        with open(OUT) as f:
            done = {(r["snapshot_date"], r["currency"]) for r in csv.DictReader(f)}
    write_header = not OUT.exists()

    with open(OUT, "a", newline="") as f:
        w = csv.DictWriter(f, fieldnames=FIELDS)
        if write_header:
            w.writeheader()
        for day in first_of_months(START):
            for currency in CURRENCIES:
                if (day.isoformat(), currency) in done:
                    continue
                t0 = time.time()
                try:
                    rows = snapshot_month(day, currency)
                except Exception as e:  # noqa: BLE001 — skip bad months, keep going
                    print(f"{day} {currency}: FAILED ({e})", file=sys.stderr)
                    continue
                w.writerows(rows)
                f.flush()
                print(
                    f"{day} {currency}: {len(rows)} near-dated calls "
                    f"({time.time() - t0:.0f}s)",
                    file=sys.stderr,
                )
                time.sleep(1)
    print(f"wrote {OUT}")


if __name__ == "__main__":
    main()
