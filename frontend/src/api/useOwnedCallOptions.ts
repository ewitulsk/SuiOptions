import { useQuery } from "@tanstack/react-query";
import { normalizeStructTag } from "@mysten/sui/utils";

import { listAllBalances, useSuiGrpcClient } from "../lib/suiGrpc";
import type { Series } from "./client";

/**
 * One bucket's worth of option coins held by the wallet. The option is a
 * per-bucket fungible `Coin<Call>`, so a holding is identified by its coin
 * type (1:1 with a bucket) and an aggregate balance — not a per-object id.
 *
 * - `coin_type` — the `Call` type arg for `bucket::exercise`; also what
 *   `coinWithBalance` selects/splits from to pay into a PTB.
 * - `bucket_id` — joins back to a bucket from `/buckets`.
 * - `amount_raw` — total balance across all the wallet's coin objects of this
 *   type, in underlying smallest-units.
 */
export type OwnedCallOption = {
  coin_type: string;
  bucket_id: string;
  amount_raw: string;
};

/**
 * Lists the wallet's option-coin holdings, mapped back to their buckets.
 *
 * Option coins live in per-roll-published packages, so there's no single
 * struct-type filter to query. Instead we read every coin balance the wallet
 * holds (gRPC `StateService.ListBalances`) and keep the ones whose type
 * matches a known bucket's `call_coin_type` (from `/buckets`). Returns an
 * empty array until both a wallet and at least one bucket coin type are known.
 */
export function useOwnedCallOptions(
  wallet: string | null,
  series: Series[] | undefined,
) {
  const client = useSuiGrpcClient();

  // Normalized option coin type -> bucket id.
  const callTypeToBucket = new Map<string, string>();
  for (const s of series ?? []) {
    for (const b of s.buckets) {
      if (b.call_coin_type) {
        callTypeToBucket.set(normalizeStructTag(b.call_coin_type), b.bucket_id);
      }
    }
  }
  // Stable key so the query refetches when the known coin-type set changes.
  const typesKey = Array.from(callTypeToBucket.keys()).sort().join(",");

  return useQuery<OwnedCallOption[], Error>({
    queryKey: ["owned-call-options", wallet, typesKey],
    enabled: wallet !== null && callTypeToBucket.size > 0,
    refetchInterval: 5_000,
    queryFn: async () => {
      if (!wallet) return [];
      const out: OwnedCallOption[] = [];
      for (const bal of await listAllBalances(client, wallet)) {
        const bucket_id = callTypeToBucket.get(normalizeStructTag(bal.coinType));
        if (!bucket_id) continue;
        if (BigInt(bal.balance) <= 0n) continue;
        out.push({
          coin_type: bal.coinType,
          bucket_id,
          amount_raw: bal.balance,
        });
      }
      return out;
    },
  });
}
