import { useQuery } from "@tanstack/react-query";

import { PACKAGE_ID } from "../config";
import { listAllOwnedObjects, useSuiGrpcClient } from "../lib/suiGrpc";

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
  const client = useSuiGrpcClient();
  return useQuery<OwnedPositionObj[], Error>({
    queryKey: ["owned-positions", wallet, PACKAGE_ID],
    enabled: wallet !== null && !!PACKAGE_ID,
    refetchInterval: 5_000,
    queryFn: async () => {
      if (!wallet || !PACKAGE_ID) return [];
      const structType = `${PACKAGE_ID}::position::Position`;
      const result: OwnedPositionObj[] = [];
      for (const obj of await listAllOwnedObjects(client, wallet, structType)) {
        const fields = obj.json as {
          bucket_id?: string;
          range_start?: string;
          range_end?: string;
        } | null;
        if (!fields?.bucket_id || fields.range_start == null || fields.range_end == null)
          continue;
        result.push({
          object_id: obj.objectId,
          bucket_id: fields.bucket_id,
          range_start_raw: fields.range_start,
          range_end_raw: fields.range_end,
        });
      }
      return result;
    },
  });
}
