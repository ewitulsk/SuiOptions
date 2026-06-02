import { useQuery } from "@tanstack/react-query";
import { useSuiClient } from "@mysten/dapp-kit";

import { PACKAGE_ID } from "../config";

/**
 * One `CallOption` object held by the user's wallet. The object id is
 * what gets passed into `bucket::exercise`; `bucket_id` joins this back
 * to a bucket from `/buckets`; `amount` is the raw `u64` underlying
 * smallest-units the call covers.
 *
 * `CallOption` is a Sui Move owned object (not a `Coin<T>`); the wallet
 * is the source of truth for current holdings. Provenance (who you
 * bought it from, what you paid) comes from `useCallTokenLots`.
 */
export type OwnedCallOption = {
  object_id: string;
  bucket_id: string;
  amount_raw: string;
};

/**
 * Lists `CallOption` objects the wallet currently holds.
 *
 * Requires a deployment for the selected environment — the struct-type
 * filter we pass to `suix_getOwnedObjects` is
 * `{package}::call_option::CallOption`, which is package-id-prefixed.
 * Returns an empty array when either `wallet` is null or no package id is
 * configured (so screens render their "no calls owned" empty state
 * instead of crashing).
 */
export function useOwnedCallOptions(wallet: string | null) {
  const client = useSuiClient();
  return useQuery<OwnedCallOption[], Error>({
    queryKey: ["owned-call-options", wallet, PACKAGE_ID],
    enabled: wallet !== null && !!PACKAGE_ID,
    refetchInterval: 5_000,
    queryFn: async () => {
      if (!wallet || !PACKAGE_ID) return [];
      const structType = `${PACKAGE_ID}::call_option::CallOption`;
      const result: OwnedCallOption[] = [];
      let cursor: string | null | undefined = undefined;
      // Paginate; in practice users hold a handful of CallOption objects
      // but the SDK enforces a page cap so we still iterate.
      do {
        const page = await client.getOwnedObjects({
          owner: wallet,
          filter: { StructType: structType },
          options: { showContent: true, showType: true },
          cursor,
        });
        for (const item of page.data) {
          const data = item.data;
          if (!data || !data.content || data.content.dataType !== "moveObject")
            continue;
          const fields = (data.content as unknown as {
            fields: { bucket_id: string; amount: string };
          }).fields;
          result.push({
            object_id: data.objectId,
            bucket_id: fields.bucket_id,
            amount_raw: fields.amount,
          });
        }
        cursor = page.hasNextPage ? page.nextCursor : undefined;
      } while (cursor);
      return result;
    },
  });
}
