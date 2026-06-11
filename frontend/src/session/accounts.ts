// On-chain reads for the session user's options Account (their custody
// vault): resolving the account from the session identity, custody coin
// balances, and custodied position ids.

import type { SuiJsonRpcClient } from "@mysten/sui/jsonRpc";

import { PACKAGE_ID } from "../config";

const ACCOUNT_CACHE_PREFIX = "tideline-session-options-account:";

/**
 * The options Account linked to a session account, or null if the user has
 * not created one yet. The account is discovered from `AccountCreated`
 * events (owner == the session account id rendered as an address) and
 * cached per session account.
 */
export async function resolveOptionsAccountId(
  client: SuiJsonRpcClient,
  sessionAccountId: string,
  sessionOwnerAddress: string,
): Promise<string | null> {
  const cacheKey = ACCOUNT_CACHE_PREFIX + sessionAccountId;
  const cached = localStorage.getItem(cacheKey);
  if (cached) return cached;
  if (!PACKAGE_ID) return null;

  let cursor: { txDigest: string; eventSeq: string } | null | undefined;
  do {
    const page = await client.queryEvents({
      query: { MoveEventType: `${PACKAGE_ID}::events::AccountCreated` },
      cursor,
      limit: 200,
    });
    for (const ev of page.data) {
      const parsed = ev.parsedJson as { account_id?: string; owner?: string };
      if (parsed?.owner === sessionOwnerAddress && parsed.account_id) {
        localStorage.setItem(cacheKey, parsed.account_id);
        return parsed.account_id;
      }
    }
    cursor = page.hasNextPage ? page.nextCursor : null;
  } while (cursor);
  return null;
}

export function cacheOptionsAccountId(sessionAccountId: string, accountId: string) {
  localStorage.setItem(ACCOUNT_CACHE_PREFIX + sessionAccountId, accountId);
}

function fieldsOf(content: unknown): Record<string, any> | null {
  const c = content as any;
  if (c && c.dataType === "moveObject") return c.fields;
  return null;
}

/**
 * Ids of every Position custodied on the options Account, from the
 * on-account `PositionIndexKey` dynamic field.
 */
export async function readCustodyPositionIds(
  client: SuiJsonRpcClient,
  optionsAccountId: string,
): Promise<string[]> {
  let cursor: string | null | undefined;
  do {
    const page = await client.getDynamicFields({
      parentId: optionsAccountId,
      cursor,
    });
    for (const entry of page.data) {
      if (!entry.name?.type?.endsWith("::account::PositionIndexKey")) continue;
      const obj = await client.getObject({
        id: entry.objectId,
        options: { showContent: true },
      });
      const value = fieldsOf(obj.data?.content)?.value;
      return Array.isArray(value) ? (value as string[]) : [];
    }
    cursor = page.hasNextPage ? page.nextCursor : null;
  } while (cursor);
  return [];
}
