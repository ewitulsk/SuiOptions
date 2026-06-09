// On-chain read helpers. The registry's `accounts` map and the account's
// `spent` ledger are Move `Table`s, i.e. collections of dynamic fields. We walk
// them to resolve a user's Account, its live generation, and per-cap spend.
//
// All reads are defensive: a brand-new user has no Account yet, so "not found"
// is a normal result (treated as generation 0 / spend 0), not an error.

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

/** Resolve the Account object id for a Solana pubkey, or null if none yet. */
export async function resolveAccountId(
  client: SuiRpcClient,
  registryId: string,
  solanaPk: Uint8Array,
): Promise<string | null> {
  const accountsTable = await tableId(client, registryId, "accounts");
  if (!accountsTable) return null;
  try {
    const entry = await client.getDynamicFieldObject({
      parentId: accountsTable,
      name: { type: "vector<u8>", value: Array.from(solanaPk) },
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
 * The `Coin<T>` balance held inside the Account's `funds` (the spendable
 * treasury), in the coin's base units. 0 if the object can't be read.
 *
 * Note this is NOT `client.getBalance({ owner: accountId })` — the funds live
 * as a `Balance<T>` field inside the shared object, not as owned coins.
 */
export async function readAccountBalance(
  client: SuiRpcClient,
  accountId: string,
): Promise<bigint> {
  const obj = await client.getObject({ id: accountId, options: { showContent: true } });
  const fields = fieldsOf(obj.data?.content);
  const funds = fields?.funds;
  if (funds == null) return 0n;
  // `Balance<T>` renders either as the raw value string or as
  // `{ fields: { value } }` depending on the node — handle both.
  if (typeof funds === "string" || typeof funds === "number") return BigInt(funds);
  return BigInt(funds.fields?.value ?? funds.value ?? 0);
}

/** Cumulative spend recorded for a cap on an Account (0 if absent). */
export async function readSpent(
  client: SuiRpcClient,
  accountId: string,
  capId: string,
): Promise<bigint> {
  const spentTable = await tableId(client, accountId, "spent");
  if (!spentTable) return 0n;
  try {
    const entry = await client.getDynamicFieldObject({
      parentId: spentTable,
      name: { type: "0x2::object::ID", value: capId },
    });
    const fields = fieldsOf(entry.data?.content);
    return BigInt(fields?.value ?? 0);
  } catch {
    return 0n;
  }
}

/** Current generation for a Solana pubkey — 0 for a never-seen user. */
export async function fetchGeneration(
  client: SuiRpcClient,
  registryId: string,
  solanaPk: Uint8Array,
): Promise<number> {
  const accountId = await resolveAccountId(client, registryId, solanaPk);
  if (!accountId) return 0;
  return readGeneration(client, accountId);
}
