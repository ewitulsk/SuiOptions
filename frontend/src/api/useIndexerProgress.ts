import { useQuery } from "@tanstack/react-query";
import { fetchIndexerProgress, type IndexerProgress } from "./client";

export function useIndexerProgress() {
  return useQuery<IndexerProgress, Error>({
    queryKey: ["indexer-progress"],
    queryFn: fetchIndexerProgress,
    // Backfill advances fast — poll often enough that the bar feels live.
    refetchInterval: 2_000,
  });
}
