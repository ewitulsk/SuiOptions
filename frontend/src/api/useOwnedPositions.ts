import { useQuery } from "@tanstack/react-query";
import { useSuiClient } from "@mysten/dapp-kit";

import { PACKAGE_ID } from "../config";

/**
 * One `Position` object held by the caller's wallet (SO-97). The wallet is
 * the source of truth for current holdings — transfer-correct and immune to
 * the api-service projection's snapshot bound, unlike `/positions`.
 *
 * `Position` is `{ bucket_id, range_start, range_end }` on chain (and is
 * never split). `object_id` is what a `redeem_position` PTB needs; the rest
 * is enriched against the indexer by object id.
 */
export type OwnedPositionObj = {
  object_id: string;
  bucket_id: string;
  range_start_raw: string;
  range_end_raw: string;
};

/**
 * Lists `Position` objects the user currently holds. Empty when `wallet`
 * is null or no package id is configured. Mirrors `useOwnedCallOptions`.
 */
export function useOwnedPositions(wallet: string | null) {
  const client = useSuiClient();
  return useQuery<OwnedPositionObj[], Error>({
    queryKey: ["owned-positions", wallet, PACKAGE_ID],
    enabled: wallet !== null && !!PACKAGE_ID,
    refetchInterval: 5_000,
    queryFn: async () => {
      if (!wallet || !PACKAGE_ID) return [];
      const structType = `${PACKAGE_ID}::position::Position`;
      const result: OwnedPositionObj[] = [];
      let cursor: string | null | undefined = undefined;
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
            fields: { bucket_id: string; range_start: string; range_end: string };
          }).fields;
          result.push({
            object_id: data.objectId,
            bucket_id: fields.bucket_id,
            range_start_raw: fields.range_start,
            range_end_raw: fields.range_end,
          });
        }
        cursor = page.hasNextPage ? page.nextCursor : undefined;
      } while (cursor);
      return result;
    },
  });
}
