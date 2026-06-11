import { useEffect, useState } from "react";
import { pyth, resolveFeedId, type PythPrice } from "./pyth";

/**
 * Subscribe to the latest Pyth price for a given symbol (e.g. `"TBTC"`) or
 * raw feed id (`"0x..."` or 64 hex chars). Returns `null` until the first
 * update arrives, and `null` if the symbol is unknown.
 */
export function usePythPrice(
  symbolOrFeedId: string | null | undefined,
): PythPrice | null {
  const feedId = symbolOrFeedId ? resolveFeedId(symbolOrFeedId) : null;

  const [price, setPrice] = useState<PythPrice | null>(() =>
    feedId ? pyth.getLast(feedId) ?? null : null,
  );

  useEffect(() => {
    if (!feedId) {
      setPrice(null);
      return;
    }
    setPrice(pyth.getLast(feedId) ?? null);
    return pyth.subscribe(feedId, setPrice);
  }, [feedId]);

  return price;
}

/**
 * Subscribe to live prices for an arbitrary set of symbols (or feed ids),
 * keyed by the symbol as passed in. Symbols that don't resolve to a feed map
 * to `null`. The active subscription set follows `symbols` — pass a memoized
 * array (or one with stable contents) to avoid needless stream churn.
 */
export function usePythPrices(
  symbols: string[],
): Record<string, PythPrice | null> {
  // Join into a stable primitive so the effect only re-runs when the set of
  // symbols actually changes, not on every parent render.
  const key = symbols.join(",");

  const [prices, setPrices] = useState<Record<string, PythPrice | null>>({});

  useEffect(() => {
    const syms = key ? key.split(",") : [];
    const initial: Record<string, PythPrice | null> = {};
    const unsubs: Array<() => void> = [];
    for (const sym of syms) {
      const feedId = resolveFeedId(sym);
      if (!feedId) {
        initial[sym] = null;
        continue;
      }
      initial[sym] = pyth.getLast(feedId) ?? null;
      unsubs.push(
        pyth.subscribe(feedId, (p) => setPrices((prev) => ({ ...prev, [sym]: p }))),
      );
    }
    setPrices(initial);
    return () => unsubs.forEach((u) => u());
  }, [key]);

  return prices;
}
