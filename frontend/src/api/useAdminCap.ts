import { useQuery } from "@tanstack/react-query";
import { useSuiClient } from "@mysten/dapp-kit";

/**
 * Detects whether the connected wallet holds an `AdminCap` and, if so,
 * returns the object id of (the first) one. Every admin PTB needs to pass
 * the `AdminCap` object as its authorizing argument, so the same query
 * both gates the admin UI and supplies the id the transactions consume.
 *
 * `AdminCap` is a plain owned object (`admin.move`); the wallet is the
 * source of truth for who is an admin. Returns `{ isAdmin: false }` when
 * the wallet is null or `VITE_PACKAGE_ID` is unset.
 */
const PACKAGE_ID: string | undefined = import.meta.env.VITE_PACKAGE_ID as
  | string
  | undefined;

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
