import { useQuery } from "@tanstack/react-query";
import type { SuiClientTypes } from "@mysten/sui/client";

import { useSuiGrpcClient, useSuiNetwork } from "../lib/suiGrpc";

export type CoinMeta = SuiClientTypes.CoinMetadata;

/**
 * Metadata (symbol, decimals) for a set of coin types, keyed by type string.
 * Types with no on-chain metadata map to null — callers fall back to the
 * type-name segment and must not assume decimals.
 */
export function useCoinMetadataMap(types: string[]) {
  const client = useSuiGrpcClient();
  const network = useSuiNetwork();
  const key = [...types].sort();
  return useQuery<Record<string, CoinMeta | null>>({
    queryKey: [network, "coinMetadata", key],
    enabled: types.length > 0,
    staleTime: Infinity,
    queryFn: async () => {
      const entries = await Promise.all(
        key.map(async (coinType) => {
          try {
            const res = await client.core.getCoinMetadata({ coinType });
            return [coinType, res.coinMetadata] as const;
          } catch {
            return [coinType, null] as const;
          }
        }),
      );
      return Object.fromEntries(entries);
    },
  });
}
