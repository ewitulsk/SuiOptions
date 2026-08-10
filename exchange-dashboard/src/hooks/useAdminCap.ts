import { useQuery } from "@tanstack/react-query";
import { useCurrentAccount } from "@mysten/dapp-kit";

import { EXCHANGE_PACKAGE_ID } from "../config";
import { listAllOwnedObjects, useSuiGrpcClient, useSuiNetwork } from "../lib/suiGrpc";

/** The exchange `admin::AdminCap` object id if the connected wallet holds one. */
export function useAdminCap() {
  const account = useCurrentAccount();
  const client = useSuiGrpcClient();
  const network = useSuiNetwork();
  return useQuery<string | null>({
    queryKey: [network, "exchangeAdminCap", account?.address, EXCHANGE_PACKAGE_ID],
    enabled: !!account && !!EXCHANGE_PACKAGE_ID,
    staleTime: 60_000,
    queryFn: async () => {
      const caps = await listAllOwnedObjects(
        client,
        account!.address,
        `${EXCHANGE_PACKAGE_ID}::admin::AdminCap`,
      );
      return caps[0]?.objectId ?? null;
    },
  });
}
