import { useQuery } from "@tanstack/react-query";
import { fetchCallTokenLots, type CallTokenLot } from "./client";

/**
 * Per-purchase provenance ("lots") for `CallOption` objects bought by
 * `wallet`. The dashboard's owned-call cards consume this to display
 * `boughtFrom`, `premiumPaid`, `boughtAt`. See `client.ts` for caveats
 * around `CallOption::split` children (their object id isn't in any
 * lot — frontend falls back to bucket-aggregate display).
 */
export function useCallTokenLots(wallet: string | null) {
  return useQuery<CallTokenLot[], Error>({
    queryKey: ["call-token-lots", wallet],
    queryFn: () =>
      wallet ? fetchCallTokenLots(wallet) : Promise.resolve([]),
    enabled: wallet !== null,
    refetchInterval: 5_000,
  });
}
