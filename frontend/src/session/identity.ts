// Unified "who is the user" hook: a connected Sui wallet (dapp-kit) or an
// active session login (siws_session). Screens that only need an address to
// query by — or to know whether on-chain actions are possible — use this
// instead of `useCurrentAccount()` directly.
//
// `address` is what queries/events attribute the user's assets by: the
// wallet address, or for sessions the session-account-id-as-address that
// owns the user's options Account.

import { useCurrentAccount } from "@mysten/dapp-kit";

import { useSession, type SessionSnapshot } from "./store";

export type UserIdentity =
  | { kind: "wallet"; address: string }
  | { kind: "session"; address: string; session: SessionSnapshot };

export function useUserIdentity(): UserIdentity | null {
  const account = useCurrentAccount();
  const session = useSession();

  // A connected Sui wallet wins: session login is the fallback for users
  // without one, and mixing the two in one browser profile is an edge case
  // the wallet resolves predictably.
  if (account) return { kind: "wallet", address: account.address };
  if (session.phase === "active" && session.ownerAddress) {
    return { kind: "session", address: session.ownerAddress, session };
  }
  return null;
}
