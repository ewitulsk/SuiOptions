import { useQuery } from "@tanstack/react-query";
import { useCurrentAccount } from "@mysten/dapp-kit";

import { listAllBalances, useSuiGrpcClient, useSuiNetwork } from "../lib/suiGrpc";

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
      return new Map(balances.map((b) => [b.coinType, BigInt(b.balance)]));
    },
  });
}
