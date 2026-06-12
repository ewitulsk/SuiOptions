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
 *
 * Session logins hold positions in their options Account custody (dynamic
 * object fields), not at an address — pass `custodyPositionIds` (from the
 * on-account index) and the hook reads those objects by id instead of
 * querying address ownership.
 */
export function useOwnedPositions(
  wallet: string | null,
  custodyPositionIds?: string[] | null,
) {
  const client = useSuiClient();
  const custodyKey = custodyPositionIds ? custodyPositionIds.slice().sort().join(",") : null;
  return useQuery<OwnedPositionObj[], Error>({
    queryKey: ["owned-positions", wallet, PACKAGE_ID, custodyKey],
    enabled: wallet !== null && !!PACKAGE_ID,
    refetchInterval: 5_000,
    queryFn: async () => {
      if (!wallet || !PACKAGE_ID) return [];
      if (custodyPositionIds) {
        if (custodyPositionIds.length === 0) return [];
        const objs = await client.multiGetObjects({
          ids: custodyPositionIds,
          options: { showContent: true },
        });
        const out: OwnedPositionObj[] = [];
        for (const item of objs) {
          const data = item.data;
          if (!data || !data.content || data.content.dataType !== "moveObject")
            continue;
          const fields = (data.content as unknown as {
            fields: { bucket_id: string; range_start: string; range_end: string };
          }).fields;
          out.push({
            object_id: data.objectId,
            bucket_id: fields.bucket_id,
            range_start_raw: fields.range_start,
            range_end_raw: fields.range_end,
          });
        }
        return out;
      }
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
