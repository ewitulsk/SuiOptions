// Pure client-side position economics for the Buy page (SO-199).
//
// No backend: aggregates the wallet's `CallTokenLot`s (one per purchase, from
// `/call-token-lots`) for a bucket and combines them with the live order-book
// mid. Lots arrive raw (atomic units) with per-lot decimals — `client.ts`'s
// `CallTokenLot` ships `amount_raw` / `premium_paid_raw`, not scaled numbers —
// so we scale here to display units (underlying tokens / settlement USD).

import { useMemo } from "react";

import type { CallTokenLot } from "./client";
import { useBars } from "./charts";

/** One lot in display units. */
export type ScaledLot = { amount: number; premiumPaid: number };

export type PositionEconomics = {
  qty: number; // Σ lot amount (underlying units) for this bucket
  totalCost: number; // Σ premium_paid
  avgPrice: number; // totalCost / qty   (0 if qty 0)
  marketValue: number; // qty * mid
  openPnl: number; // marketValue − totalCost
  openPnlPct: number; // openPnl / totalCost
  positionRatio: number; // this bucket's qty / total qty held across buckets (0..1)
};

/** Raw smallest-units → display units. `null` decimals → unscaled. */
function scale(raw: string, decimals: number | null): number {
  return decimals == null ? Number(raw) : Number(raw) / 10 ** decimals;
}

/** Scale a raw `CallTokenLot` into display-unit amount + premium. */
export function scaleLot(lot: CallTokenLot): ScaledLot {
  return {
    amount: scale(lot.amount_raw, lot.asset_decimals),
    premiumPaid: scale(lot.premium_paid_raw, lot.settlement_decimals),
  };
}

/**
 * Combine a bucket's scaled lots with the live mid. `totalHeldQtyAllBuckets` is
 * the wallet's qty across every bucket, so `positionRatio` reflects how much of
 * the book this strike represents.
 */
export function deriveEconomics(
  lotsForBucket: ScaledLot[],
  mid: number,
  totalHeldQtyAllBuckets: number,
): PositionEconomics {
  const qty = lotsForBucket.reduce((s, l) => s + l.amount, 0);
  const totalCost = lotsForBucket.reduce((s, l) => s + l.premiumPaid, 0);
  const avgPrice = qty > 0 ? totalCost / qty : 0;
  const marketValue = qty * mid;
  const openPnl = marketValue - totalCost;
  return {
    qty,
    totalCost,
    avgPrice,
    marketValue,
    openPnl,
    openPnlPct: totalCost > 0 ? openPnl / totalCost : 0,
    positionRatio: totalHeldQtyAllBuckets > 0 ? qty / totalHeldQtyAllBuckets : 0,
  };
}

/**
 * Derive economics for one bucket from the full lot set. Scales every lot once
 * (for the cross-bucket qty denominator) and the bucket's own subset.
 */
export function usePositionEconomics(
  allLots: CallTokenLot[],
  bucketId: string | null,
  mid: number | null,
): PositionEconomics {
  return useMemo(() => {
    const m = mid ?? 0;
    const totalHeld = allLots.reduce((s, l) => s + scaleLot(l).amount, 0);
    const forBucket = allLots
      .filter((l) => l.bucket_id === bucketId)
      .map(scaleLot);
    return deriveEconomics(forBucket, m, totalHeld);
  }, [allLots, bucketId, mid]);
}

// ── Day's P&L reference prices ────────────────────────────────────────────────
//
// Option leg only: the position's value change since the current UTC day's
// session open. We use the option pool's `1d` trade bar `open` as the
// session-open reference (the charts service exposes OHLC trade bars and a
// midpoint line per pool — there is no per-day mid OPEN, so the trade open is
// the closest available session anchor). `null` until a current-day bar exists.
//
// There is no client-side daily history for the UNDERLYING spot (Pyth streams
// live spot only; the charts service is keyed by option pool, not the spot
// feed), so an underlying day-change is not derivable here yet — the panel
// shows live spot without a fabricated delta.

export type DayChange = {
  /** Option-leg P&L since session open, in settlement units. `null` if no bar. */
  optionPnl: number | null;
};

const DAY_MS = 24 * 60 * 60 * 1000;

export function useDayChange(
  poolId: string | null,
  qty: number,
  mid: number | null,
): DayChange {
  const { bars } = useBars(poolId, "1d");
  return useMemo(() => {
    if (mid == null || qty <= 0) return { optionPnl: null };
    const dayStart = Math.floor(Date.now() / DAY_MS) * DAY_MS;
    const todays = bars.find((b) => b.t >= dayStart);
    if (!todays) return { optionPnl: null };
    return { optionPnl: qty * (mid - todays.o) };
  }, [bars, qty, mid]);
}
