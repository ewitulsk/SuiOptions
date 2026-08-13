// Live state of the ONE ingress whitelist (guarded launch): the standalone
// whitelist package's shared `Whitelist` object — `members: VecSet<address>`
// plus the enforcement / pause flags. Direct chain read — no service serves
// this.

import { useQuery } from "@tanstack/react-query";
import { normalizeSuiAddress } from "@mysten/sui/utils";

import { WHITELIST_ID } from "../config";
import { useSuiGrpcClient } from "../lib/suiGrpc";
import { asRecord, structFields, vecSetItems } from "./vaultHoldings";

export type WhitelistState = {
  members: string[];
  whitelistEnabled: boolean;
  ingressPaused: boolean;
};

export function useWhitelist() {
  const client = useSuiGrpcClient();
  return useQuery<WhitelistState, Error>({
    queryKey: ["ingress-whitelist", WHITELIST_ID],
    enabled: !!WHITELIST_ID,
    // Only moves on admin action; the section refetches after every tx.
    refetchInterval: 30_000,
    queryFn: async () => {
      const { object } = await client.core.getObject({
        objectId: WHITELIST_ID as string,
        include: { json: true },
      });
      const f = structFields(object.json) ?? asRecord(object.json);
      return {
        // VecSet renders as `{ contents: [addr, …] }` (or a bare array).
        members: vecSetItems(f?.members)
          .filter((m): m is string => typeof m === "string")
          .map((m) => normalizeSuiAddress(m)),
        whitelistEnabled: f?.whitelist_enabled === true,
        ingressPaused: f?.ingress_paused === true,
      };
    },
  });
}
