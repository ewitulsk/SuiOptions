import { useQuery } from "@tanstack/react-query";
import { normalizeStructTag } from "@mysten/sui/utils";

import { listAllBalances, useSuiGrpcClient } from "../lib/suiGrpc";
import { optionCoinType, seriesOptionType, type Series } from "./client";
import type { OwnedCallOption } from "./useOwnedCallOptions";

/**
 * Mirror of {@link useOwnedCallOptions} for cash-secured PUTs. Lists the
 * wallet's `Coin<Put>` holdings, mapped back to their put buckets.
 *
 * Identical mechanics to the call hook — read every coin balance and keep the
 * ones whose type matches a known put bucket's option coin type — but the
 * coin-type→bucket map is built only from series whose `option_type` is
 * `"put"`. Returns the same `OwnedCallOption` row shape (`coin_type` here is a
 * `Put` type arg). Empty until both a wallet and a put coin type are known.
 */
export function useOwnedPutOptions(
  wallet: string | null,
  series: Series[] | undefined,
) {
  const client = useSuiGrpcClient();

  // Normalized put option coin type -> bucket id (put series only).
  const putTypeToBucket = new Map<string, string>();
  for (const s of series ?? []) {
    if (seriesOptionType(s) !== "put") continue;
    for (const b of s.buckets) {
      const ct = optionCoinType(b);
      // Uncreated strikes have no bucket object to own coins against.
      if (ct && b.bucket_id) putTypeToBucket.set(normalizeStructTag(ct), b.bucket_id);
    }
  }
  const typesKey = Array.from(putTypeToBucket.keys()).sort().join(",");

  return useQuery<OwnedCallOption[], Error>({
    queryKey: ["owned-put-options", wallet, typesKey],
    enabled: wallet !== null && putTypeToBucket.size > 0,
    refetchInterval: 5_000,
    queryFn: async () => {
      if (!wallet) return [];
      const out: OwnedCallOption[] = [];
      for (const bal of await listAllBalances(client, wallet)) {
        const bucket_id = putTypeToBucket.get(normalizeStructTag(bal.coinType));
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
