import { useQuery } from "@tanstack/react-query";
import { useSuiClient } from "@mysten/dapp-kit";
import { normalizeStructTag } from "@mysten/sui/utils";

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
 * holds (`getAllBalances`) and keep the ones whose type matches a known
 * bucket's `call_coin_type` (from `/buckets`). Returns an empty array until
 * both a wallet and at least one bucket coin type are known.
 */
export function useOwnedCallOptions(
  wallet: string | null,
  series: Series[] | undefined,
  /** Session custody balances (canonical type → raw) — used instead of
   *  address-owned balances when the user is a session login. */
  custodyBalances?: Record<string, bigint> | null,
) {
  const client = useSuiClient();

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

  const custodyKey = custodyBalances
    ? Object.entries(custodyBalances)
        .map(([t, v]) => `${t}:${v}`)
        .sort()
        .join(",")
    : null;

  return useQuery<OwnedCallOption[], Error>({
    queryKey: ["owned-call-options", wallet, typesKey, custodyKey],
    enabled: wallet !== null && callTypeToBucket.size > 0,
    refetchInterval: 5_000,
    queryFn: async () => {
      if (!wallet) return [];
      if (custodyBalances) {
        const out: OwnedCallOption[] = [];
        for (const [type, raw] of Object.entries(custodyBalances)) {
          const bucket_id = callTypeToBucket.get(normalizeStructTag(type));
          if (!bucket_id || raw <= 0n) continue;
          out.push({ coin_type: normalizeStructTag(type), bucket_id, amount_raw: raw.toString() });
        }
        return out;
      }
      const balances = await client.getAllBalances({ owner: wallet });
      const out: OwnedCallOption[] = [];
      for (const bal of balances) {
        const bucket_id = callTypeToBucket.get(normalizeStructTag(bal.coinType));
        if (!bucket_id) continue;
        if (BigInt(bal.totalBalance) <= 0n) continue;
        out.push({
          coin_type: bal.coinType,
          bucket_id,
          amount_raw: bal.totalBalance,
        });
      }
      return out;
    },
  });
}
