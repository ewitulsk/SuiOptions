import { useQuery } from "@tanstack/react-query";

import { getMarkets, type Market } from "../api/orderbook";

/** Markets are static per orderbook boot (loaded from deployments.json). */
export function useMarkets() {
  return useQuery<Market[]>({
    queryKey: ["orderbook", "markets"],
    queryFn: getMarkets,
    staleTime: Infinity,
    retry: 1,
  });
}

/** registryId → Market lookup for the route assembler. */
export function marketsById(markets: Market[]): Map<string, Market> {
  return new Map(markets.map((m) => [m.registryId, m]));
}

/** Distinct tradeable token types (union of every market's base/quote). */
export function tokenUniverse(markets: Market[]): string[] {
  return [...new Set(markets.flatMap((m) => [m.base, m.quote]))];
}
