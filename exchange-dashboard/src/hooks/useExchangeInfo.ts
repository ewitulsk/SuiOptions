import { useQuery } from "@tanstack/react-query";

import { TOKEN_INFO_URL } from "../config";

/** The `exchange` block subset of token-info's `/package-info`. */
export interface ExchangeInfo {
  packageId: string;
  adminCapId: string;
  /** Shared ingress `Whitelist` (guarded launch, SO-384). `null` on records
   * predating the whitelist module — fills can't be built without it. */
  whitelistId: string | null;
}

/**
 * The current exchange deployment, from token-info. Fetched at runtime —
 * never baked into the build — because the package (and every market
 * registry) is republished on each contract redeploy. `null` = the env has
 * no exchange block; screens show their writes-disabled hint.
 */
export function useExchangeInfo() {
  return useQuery<ExchangeInfo | null>({
    queryKey: ["tokenInfo", "exchange"],
    queryFn: async () => {
      const res = await fetch(`${TOKEN_INFO_URL}/package-info`);
      if (!res.ok) throw new Error(`token-info /package-info: HTTP ${res.status}`);
      const body = await res.json();
      const ex = body?.exchange;
      return ex
        ? {
            packageId: ex.packageId,
            adminCapId: ex.adminCapId,
            // SO-384: the shared ingress Whitelist moved to the record's
            // top-level standalone whitelist block.
            whitelistId: body?.whitelist?.whitelistId ?? null,
          }
        : null;
    },
    staleTime: Infinity,
    retry: 1,
  });
}
