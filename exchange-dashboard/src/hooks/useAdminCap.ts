import { useQuery } from "@tanstack/react-query";
import { useCurrentAccount } from "@mysten/dapp-kit";

import { listAllOwnedObjects, useSuiGrpcClient, useSuiNetwork } from "../lib/suiGrpc";

/**
 * The exchange `admin::AdminCap` object id if the connected wallet holds
 * one. `packageId` comes from token-info at runtime (useExchangeInfo).
 */
export function useAdminCap(packageId: string | undefined) {
  const account = useCurrentAccount();
  const client = useSuiGrpcClient();
  const network = useSuiNetwork();
  return useQuery<string | null>({
    queryKey: [network, "exchangeAdminCap", account?.address, packageId],
    enabled: !!account && !!packageId,
    staleTime: 60_000,
    queryFn: async () => {
      const caps = await listAllOwnedObjects(
        client,
        account!.address,
        `${packageId}::admin::AdminCap`,
      );
      return caps[0]?.objectId ?? null;
    },
  });
}
