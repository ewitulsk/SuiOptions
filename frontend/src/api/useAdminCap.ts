import { useQuery } from "@tanstack/react-query";
import { useSuiClient } from "@mysten/dapp-kit";

import { PACKAGE_ID } from "../config";

/**
 * Detects whether the connected wallet holds an `AdminCap` and, if so,
 * returns the object id of (the first) one. Every admin PTB needs to pass
 * the `AdminCap` object as its authorizing argument, so the same query
 * both gates the admin UI and supplies the id the transactions consume.
 *
 * `AdminCap` is a plain owned object (`admin.move`); the wallet is the
 * source of truth for who is an admin. Returns `{ isAdmin: false }` when
 * the wallet is null or the selected environment has no deployment.
 */

export type AdminCapStatus = {
  isAdmin: boolean;
  adminCapId: string | null;
};

export function useAdminCap(wallet: string | null) {
  const client = useSuiClient();
  return useQuery<AdminCapStatus, Error>({
    queryKey: ["admin-cap", wallet, PACKAGE_ID],
    enabled: wallet !== null && !!PACKAGE_ID,
    // Admin status almost never changes mid-session; a slow poll is plenty.
    refetchInterval: 30_000,
    queryFn: async () => {
      if (!wallet || !PACKAGE_ID) return { isAdmin: false, adminCapId: null };
      const structType = `${PACKAGE_ID}::admin::AdminCap`;
      const page = await client.getOwnedObjects({
        owner: wallet,
        filter: { StructType: structType },
        options: { showType: false },
      });
      const first = page.data.find((item) => item.data?.objectId);
      const adminCapId = first?.data?.objectId ?? null;
      return { isAdmin: adminCapId !== null, adminCapId };
    },
  });
}

/** One `OrgCap` held by the connected wallet, with the org it authorizes. */
export type OwnedOrgCap = {
  /** The OrgCap object id (the authorizing PTB argument). */
  capId: string;
  /** The shared Org object this cap administers. */
  orgId: string;
};

/**
 * All `OrgCap`s the connected wallet holds. Orgs are permissionless — any
 * wallet may hold several caps (one per org it created or was transferred).
 * Each cap's `org_id` field is read from object content so the org admin UI
 * can match caps to the buckets/series they govern.
 */
export function useOrgCaps(wallet: string | null) {
  const client = useSuiClient();
  return useQuery<OwnedOrgCap[], Error>({
    queryKey: ["org-caps", wallet, PACKAGE_ID],
    enabled: wallet !== null && !!PACKAGE_ID,
    refetchInterval: 30_000,
    queryFn: async () => {
      if (!wallet || !PACKAGE_ID) return [];
      const structType = `${PACKAGE_ID}::org::OrgCap`;
      const page = await client.getOwnedObjects({
        owner: wallet,
        filter: { StructType: structType },
        options: { showContent: true },
      });
      const caps: OwnedOrgCap[] = [];
      for (const item of page.data) {
        const capId = item.data?.objectId;
        const content = item.data?.content;
        if (!capId || content?.dataType !== "moveObject") continue;
        const fields = content.fields as { org_id?: string };
        if (fields.org_id) caps.push({ capId, orgId: fields.org_id });
      }
      return caps;
    },
  });
}
