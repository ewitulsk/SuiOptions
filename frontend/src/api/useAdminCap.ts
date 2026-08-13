import { useQuery } from "@tanstack/react-query";

import { EXCHANGE_PACKAGE_ID, PACKAGE_ID, WHITELIST_PACKAGE_ID } from "../config";
import { useSuiGrpcClient } from "../lib/suiGrpc";

/**
 * Detects whether the connected wallet holds an `AdminCap` and, if so,
 * returns the object id of (the first) one. Every admin PTB needs to pass
 * the `AdminCap` object as its authorizing argument, so the same query
 * both gates the admin UI and supplies the id the transactions consume.
 *
 * `AdminCap` is a plain owned object (`admin.move`); the wallet is the
 * source of truth for who is an admin. Returns `{ isAdmin: false }` when
 * the wallet is null or the selected environment has no deployment.
 *
 * Three caps are discovered the same way (owned-objects lookup by type):
 *   - core `admin::AdminCap` — gates the admin UI and most PTBs
 *   - exchange `admin::AdminCap` — the per-market pause legs
 *   - whitelist `whitelist::AdminCap` (standalone package) — ingress
 *     whitelist mutations. Config also serves the cap id statically
 *     (`WHITELIST_ADMIN_CAP_ID`), but the ownership lookup is preferred so
 *     the render-guard reflects what THIS wallet can actually sign for.
 */

export type AdminCapStatus = {
  isAdmin: boolean;
  adminCapId: string | null;
  /** Owned `exchange::admin::AdminCap`; null when not held or no exchange
   * is deployed. */
  exchangeAdminCapId: string | null;
  /** Owned standalone `whitelist::AdminCap`; null when not held or no
   * whitelist package is deployed. */
  whitelistAdminCapId: string | null;
};

export function useAdminCap(wallet: string | null) {
  const client = useSuiGrpcClient();
  return useQuery<AdminCapStatus, Error>({
    queryKey: ["admin-cap", wallet, PACKAGE_ID, EXCHANGE_PACKAGE_ID, WHITELIST_PACKAGE_ID],
    enabled: wallet !== null && !!PACKAGE_ID,
    // Admin status almost never changes mid-session; a slow poll is plenty.
    refetchInterval: 30_000,
    queryFn: async () => {
      const none: AdminCapStatus = {
        isAdmin: false,
        adminCapId: null,
        exchangeAdminCapId: null,
        whitelistAdminCapId: null,
      };
      if (!wallet || !PACKAGE_ID) return none;
      const findCap = async (type: string): Promise<string | null> => {
        const page = await client.core.listOwnedObjects({
          owner: wallet,
          type,
          limit: 1,
        });
        return page.objects[0]?.objectId ?? null;
      };
      const [adminCapId, exchangeAdminCapId, whitelistAdminCapId] = await Promise.all([
        findCap(`${PACKAGE_ID}::admin::AdminCap`),
        EXCHANGE_PACKAGE_ID
          ? findCap(`${EXCHANGE_PACKAGE_ID}::admin::AdminCap`)
          : Promise.resolve(null),
        WHITELIST_PACKAGE_ID
          ? findCap(`${WHITELIST_PACKAGE_ID}::whitelist::AdminCap`)
          : Promise.resolve(null),
      ]);
      return {
        isAdmin: adminCapId !== null,
        adminCapId,
        exchangeAdminCapId,
        whitelistAdminCapId,
      };
    },
  });
}
