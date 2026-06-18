import { useQuery } from "@tanstack/react-query";
import { fetchBuckets, type Series } from "./client";

export function useBuckets(opts?: { excludeExpired?: boolean }) {
  const excludeExpired = opts?.excludeExpired ?? false;
  return useQuery<Series[], Error>({
    // Keep the filtered and unfiltered catalogs in separate cache entries.
    queryKey: ["buckets", excludeExpired],
    queryFn: () => fetchBuckets({ excludeExpired }),
    // Buckets change on chain — refresh every few seconds while a screen is open.
    refetchInterval: 5_000,
  });
}
