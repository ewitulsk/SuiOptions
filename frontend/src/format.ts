// Format a USD-denominated price. Prices >= $1 show two decimals; sub-dollar
// prices show up to four decimals (down to hundredths of a cent); sub-penny
// values (0 < |v| < 0.01) extend to two significant figures so they never
// collapse to "0.00" (SO-166). Returns the bare number — callers add the
// currency symbol/label. Token quantities and percentages are not prices and
// must not use this.
export function formatPrice(value: number, opts?: { grouping?: boolean }): string {
  if (!Number.isFinite(value)) return "—";
  const useGrouping = opts?.grouping ?? false;
  const abs = Math.abs(value);
  let minimumFractionDigits = 2;
  let maximumFractionDigits = 2;
  if (abs > 0 && abs < 1) {
    if (abs < 0.01) {
      // Pin to the decimal place that yields two significant figures so the
      // trailing zero survives (0.004 -> "0.0040", not "0.004").
      const decimals = Math.min(8, -Math.floor(Math.log10(abs)) + 1);
      minimumFractionDigits = decimals;
      maximumFractionDigits = decimals;
    } else {
      maximumFractionDigits = 4;
    }
  }
  return value.toLocaleString("en-US", {
    useGrouping,
    minimumFractionDigits,
    maximumFractionDigits,
  });
}

// A premium hero shows the exact amount the user transacts — at least two
// decimals, but never rounded away to the nearest cent (full settlement
// precision). Trailing zeros past the second place are trimmed.
export function formatPremiumFull(value: number, opts?: { grouping?: boolean }): string {
  if (!Number.isFinite(value)) return "—";
  return value.toLocaleString("en-US", {
    useGrouping: opts?.grouping ?? false,
    minimumFractionDigits: 2,
    maximumFractionDigits: 8,
  });
}
