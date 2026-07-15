import { useQuery } from "@tanstack/react-query";

import { fetchBridgeTransfers, type BridgeTransfer } from "./bridge";

/**
 * Transfers for every connected wallet (Sui and/or Solana), merged and
 * newest-first. Polls while mounted so in-flight statuses stay live.
 */
export function useBridgeTransfers(wallets: string[]) {
  return useQuery<BridgeTransfer[], Error>({
    queryKey: ["bridge-transfers", ...wallets],
    enabled: wallets.length > 0,
    queryFn: async () => {
      const lists = await Promise.all(wallets.map(fetchBridgeTransfers));
      const byId = new Map<number, BridgeTransfer>();
      for (const list of lists) for (const t of list) byId.set(t.id, t);
      return [...byId.values()].sort((a, b) => b.created_at_ms - a.created_at_ms);
    },
    refetchInterval: 5_000,
  });
}
