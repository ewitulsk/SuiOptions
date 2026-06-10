import { useQuery } from "@tanstack/react-query";

import { fetchOrgs, type VerifiedOrg } from "./client";

/**
 * The verified-orgs allowlist, from api-service `GET /orgs` (the same set
 * `/buckets` is filtered by). Used to label series with their org name and
 * to power org filters. Changes only on platform-admin action — a slow poll
 * is plenty.
 */
export function useVerifiedOrgs() {
  return useQuery<VerifiedOrg[], Error>({
    queryKey: ["verified-orgs"],
    refetchInterval: 60_000,
    queryFn: fetchOrgs,
  });
}
