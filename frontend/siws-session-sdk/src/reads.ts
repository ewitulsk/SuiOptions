// On-chain read helpers. The registry's `accounts` map and the account's
// `spent` ledger are Move `Table`s, i.e. collections of dynamic fields; the
// account's balances are `Balance<T>` dynamic fields keyed by
// `account::BalanceKey<T>`. We walk them to resolve a user's Account, its live
// generation, per-type balances, and per-(cap, type) spend.
//
// All reads are defensive: a brand-new user has no Account yet, so "not found"
// is a normal result (treated as generation 0 / spend 0), not an error.

import { normalizeStructTag } from "@mysten/sui/utils";

import type { SuiRpcClient } from "./client.js";

function fieldsOf(content: unknown): Record<string, any> | null {
  const c = content as any;
  if (c && c.dataType === "moveObject") return c.fields;
  return null;
}

/** Read the inner `id` of a `Table`/`Bag` stored at `parent.<field>`. */
async function tableId(
  client: SuiRpcClient,
  parentId: string,
  fieldName: string,
): Promise<string | null> {
  const obj = await client.getObject({ id: parentId, options: { showContent: true } });
  const fields = fieldsOf(obj.data?.content);
  const table = fields?.[fieldName];
  return table?.fields?.id?.id ?? null;
}

/** Resolve the Account object id for a root identity, or null if none yet. */
export async function resolveAccountId(
  client: SuiRpcClient,
  registryId: string,
  identity: Uint8Array,
): Promise<string | null> {
  const accountsTable = await tableId(client, registryId, "accounts");
  if (!accountsTable) return null;
  try {
    const entry = await client.getDynamicFieldObject({
      parentId: accountsTable,
      name: { type: "vector<u8>", value: Array.from(identity) },
    });
    const fields = fieldsOf(entry.data?.content);
    // Table<vector<u8>, ID> -> Field.value is the account object id.
    return (fields?.value as string) ?? null;
  } catch {
    return null;
  }
}

/** Live `generation` of an Account (0 if the object can't be read). */
export async function readGeneration(
  client: SuiRpcClient,
  accountId: string,
): Promise<number> {
  const obj = await client.getObject({ id: accountId, options: { showContent: true } });
  const fields = fieldsOf(obj.data?.content);
  return Number(fields?.generation ?? 0);
}

/**
 * All coin balances held inside the Account, keyed by canonical coin type.
 * The balances are `Balance<T>` dynamic fields on the account object itself
 * (NOT owned coins — `client.getBalance({ owner: accountId })` sees nothing).
 */
export async function readAccountBalances(
  client: SuiRpcClient,
  accountId: string,
): Promise<Record<string, bigint>> {
  const out: Record<string, bigint> = {};
  let cursor: string | null | undefined = undefined;
  do {
    const page = await client.getDynamicFields({ parentId: accountId, cursor });
    for (const entry of page.data) {
      const m = /::account::BalanceKey<(.+)>$/.exec(entry.name?.type ?? "");
      if (!m) continue;
      const obj = await client.getObject({
        id: entry.objectId,
        options: { showContent: true },
      });
      const fields = fieldsOf(obj.data?.content);
      const value = fields?.value;
      const raw =
        typeof value === "string" || typeof value === "number"
          ? value
          : (value?.fields?.value ?? value?.value ?? 0);
      out[normalizeStructTag(m[1])] = BigInt(raw);
    }
    cursor = page.hasNextPage ? page.nextCursor : null;
  } while (cursor);
  return out;
}

/** Balance of one coin type held inside the Account (0 when absent). */
export async function readAccountBalance(
  client: SuiRpcClient,
  accountId: string,
  coinType: string,
): Promise<bigint> {
  const balances = await readAccountBalances(client, accountId);
  return balances[normalizeStructTag(coinType)] ?? 0n;
}

/**
 * Cumulative spend recorded for (cap, coin type) on an Account (0 if absent).
 * `packageId` must be the package id that DEFINES `account::SpendKey` (the
 * original publish id — equal to the current id until the package is
 * upgraded).
 */
export async function readSpent(
  client: SuiRpcClient,
  packageId: string,
  accountId: string,
  capId: string,
  coinType: string,
): Promise<bigint> {
  const spentTable = await tableId(client, accountId, "spent");
  if (!spentTable) return 0n;
  try {
    const canonical = normalizeStructTag(coinType);
    const entry = await client.getDynamicFieldObject({
      parentId: spentTable,
      name: {
        type: `${packageId}::account::SpendKey`,
        value: {
          cap_id: capId,
          coin_type: Array.from(new TextEncoder().encode(canonical)),
        },
      },
    });
    const fields = fieldsOf(entry.data?.content);
    return BigInt(fields?.value ?? 0);
  } catch {
    return 0n;
  }
}

/** Current generation for a root identity — 0 for a never-seen user. */
export async function fetchGeneration(
  client: SuiRpcClient,
  registryId: string,
  identity: Uint8Array,
): Promise<number> {
  const accountId = await resolveAccountId(client, registryId, identity);
  if (!accountId) return 0;
  return readGeneration(client, accountId);
}
