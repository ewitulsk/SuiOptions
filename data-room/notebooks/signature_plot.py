#!/usr/bin/env python3
"""Volatility signature plot (spec §8 acceptance analysis).

Plots RV vs sampling interval per (source, estimator) from the gold `rv`
table. Where the curve flattens is the finest defensible sampling
interval; a curve that keeps rising as the interval shrinks is measuring
microstructure noise, not volatility.

Usage:
    DATA_ROOM=s3://<bucket> python notebooks/signature_plot.py \
        [--instrument btc-usdc.binance] [--window 86400] [--date-from 2026-08-01]

Deps: pip install duckdb matplotlib
"""

import argparse
import os

import duckdb
import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--instrument", default="btc-usdc.binance")
    ap.add_argument("--window", type=int, default=86_400, help="window_s to plot")
    ap.add_argument("--date-from", default="1970-01-01")
    ap.add_argument("--min-coverage", type=float, default=0.9)
    ap.add_argument("--out", default="signature_plot.png")
    args = ap.parse_args()

    root = os.environ["DATA_ROOM"]
    con = duckdb.connect()
    df = con.execute(
        f"""
        SELECT sample_interval_s, source, estimator,
               avg(sigma_ann) AS sigma, count(*) AS n
        FROM read_parquet('{root}/gold/v1/rv/**/*.parquet', hive_partitioning=true)
        WHERE instrument_id = ?
          AND window_s = ?
          AND date >= ?
          AND coverage >= ?
        GROUP BY 1, 2, 3
        ORDER BY 1
        """,
        [args.instrument, args.window, args.date_from, args.min_coverage],
    ).df()

    if df.empty:
        raise SystemExit("no rv rows matched — run `gold rv` first / relax filters")

    fig, ax = plt.subplots(figsize=(8, 5))
    for (source, estimator), g in df.groupby(["source", "estimator"]):
        ax.plot(g.sample_interval_s, g.sigma, marker="o", label=f"{source}/{estimator}")
    ax.set_xscale("log")
    ax.set_xlabel("sampling interval (s, log)")
    ax.set_ylabel("annualized RV (mean over window-ends)")
    ax.set_title(f"Volatility signature — {args.instrument}, window={args.window}s")
    ax.legend()
    ax.grid(True, alpha=0.3)
    fig.tight_layout()
    fig.savefig(args.out, dpi=150)
    print(f"wrote {args.out}")
    print(df.to_string(index=False))


if __name__ == "__main__":
    main()
