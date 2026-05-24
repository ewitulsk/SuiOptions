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
