import { useQuery } from "@tanstack/react-query";
import { normalizeStructTag } from "@mysten/sui/utils";

import { useSuiGrpcClient } from "../lib/suiGrpc";

/**
 * The wallet's total balance of a given coin type, in raw smallest-units
 * (a u64 decimal string from gRPC `StateService.GetBalance`). Returns `"0"`
 * while disabled (no wallet / no coin type). Mirrors the `enabled` +
 * `refetchInterval` shape of `useOwnedCallOptions`.
 *
 * Scale by the coin's decimals at the call site for display.
 */
export function useCoinBalance(owner: string | null, coinType: string | null) {
  const client = useSuiGrpcClient();
  return useQuery<string, Error>({
    queryKey: ["coin-balance", owner, coinType],
    enabled: owner !== null && coinType !== null,
    refetchInterval: 5_000,
    queryFn: async () => {
      if (!owner || !coinType) return "0";
      // A coin type whose address lacks the `0x` prefix (e.g. a raw chain
      // `TypeName` like `9b72…::tbtc::TBTC`) is rejected — and unlike the
      // transaction builder, `getBalance` does not normalize its argument.
      // Canonicalize defensively so any source resolves correctly.
      const bal = await client.core.getBalance({
        owner,
        coinType: normalizeStructTag(coinType),
      });
      return bal.balance.balance;
    },
  });
}
