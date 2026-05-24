import { useQuery } from "@tanstack/react-query";
import { fetchBuckets, type Series } from "./client";

export function useBuckets() {
  return useQuery<Series[], Error>({
    queryKey: ["buckets"],
    queryFn: fetchBuckets,
    // Buckets change on chain — refresh every few seconds while a screen is open.
    refetchInterval: 5_000,
  });
}
