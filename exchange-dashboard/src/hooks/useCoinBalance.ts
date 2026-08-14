import { useQuery } from "@tanstack/react-query";
import { useCurrentAccount } from "@mysten/dapp-kit";

import { normalizeStructTag } from "@mysten/sui/utils";

import { listAllBalances, useSuiGrpcClient, useSuiNetwork } from "../lib/suiGrpc";

/** Canonicalize a coin type for map keys — raw wallet strings and market
 * config strings must collide (nested generic types included). */
export function balanceKey(coinType: string): string {
  try {
    return normalizeStructTag(coinType);
  } catch {
    return coinType;
  }
}

/** Connected wallet's balances as coin type → raw amount. */
export function useWalletBalances() {
  const account = useCurrentAccount();
  const client = useSuiGrpcClient();
  const network = useSuiNetwork();
  return useQuery<Map<string, bigint>>({
    queryKey: [network, "balances", account?.address],
    enabled: !!account,
    refetchInterval: 15_000,
    queryFn: async () => {
      const balances = await listAllBalances(client, account!.address);
      return new Map(balances.map((b) => [balanceKey(b.coinType), BigInt(b.balance)]));
    },
  });
}
