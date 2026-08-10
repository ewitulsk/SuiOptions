import { useQuery } from "@tanstack/react-query";
import { useEffect, useState } from "react";

import { getRoutes, type RouteResponse } from "../api/orderbook";

/** Debounce a fast-changing value (amount input) before it hits the API. */
export function useDebounced<T>(value: T, ms = 400): T {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    const t = setTimeout(() => setDebounced(value), ms);
    return () => clearTimeout(t);
  }, [value, ms]);
  return debounced;
}

/**
 * Route quote for swapping `amount` of `from` into `to`. Refetches every 5s —
 * books move, and the signed fill tickets in the response go stale.
 */
export function useRouteQuote(from: string | null, to: string | null, amount: bigint | null) {
  const enabled = !!from && !!to && from !== to && amount !== null && amount > 0n;
  return useQuery<RouteResponse>({
    queryKey: ["orderbook", "routes", from, to, amount?.toString()],
    queryFn: () => getRoutes(from!, to!, amount!),
    enabled,
    refetchInterval: 5_000,
    // NO_ROUTE etc. are deterministic 4xx responses — retrying won't help.
    retry: false,
  });
}
