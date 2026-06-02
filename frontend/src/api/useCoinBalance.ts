import { useQuery } from "@tanstack/react-query";
import { useSuiClient } from "@mysten/dapp-kit";

/**
 * The wallet's total balance of a given coin type, in raw smallest-units
 * (a u64 decimal string from `suix_getBalance`). Returns `"0"` while
 * disabled (no wallet / no coin type). Mirrors the `enabled` +
 * `refetchInterval` shape of `useOwnedCallOptions`.
 *
 * Scale by the coin's decimals at the call site for display.
 */
export function useCoinBalance(owner: string | null, coinType: string | null) {
  const client = useSuiClient();
  return useQuery<string, Error>({
    queryKey: ["coin-balance", owner, coinType],
    enabled: owner !== null && coinType !== null,
    refetchInterval: 5_000,
    queryFn: async () => {
      if (!owner || !coinType) return "0";
      const bal = await client.getBalance({ owner, coinType });
      return bal.totalBalance;
    },
  });
}
