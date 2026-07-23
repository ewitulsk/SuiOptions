// DeepBook-Margin equity-leg discovery for the appraisal composer (SO-299
// phase C). A vault pinning `dbm_oracle::DbmOracle` records its external
// account's equity via `dbm_oracle::record{,_no_debt}`, which needs the
// account's `MarginManager`, its DeepBook pool, the borrowed side's
// `MarginPool`, and Pyth attestations for the pool's base/quote legs.
//
// Everything resolves through `getObject` on dynamic-field ids DERIVED
// client-side (`deriveDynamicFieldID`) — never the RPC dynamic-field index
// (`getDynamicFieldObject`/`getDynamicFields`), which is broken on our RPC
// provider. Chain layout walked:
//
//   MarginRegistry (outer, pinned id)
//     .inner: Versioned ── df[u64 = 1] → MarginRegistryInner
//       .margin_managers: Table<address, VecSet<ID>> ── df[account] → manager id
//       .pool_registry:   Table<ID, PoolConfig>      ── df[pool id] → margin pools
//     ── df[ConfigKey<PythConfig>] → currencies: VecMap<TypeName, CoinTypeData>
//   Pyth price_info Table (pinned id) ── df[PriceIdentifier] → PriceInfoObject id
//
// This module is deliberately config-free (ids come in as a parameter) so
// the standalone discovery script under `scripts/` can drive it from node,
// where `config.ts` (import.meta.env) can't load.

import { bcs } from "@mysten/sui/bcs";
import type { SuiGrpcClient } from "@mysten/sui/grpc";
import {
  deriveDynamicFieldID,
  fromBase64,
  fromHex,
  normalizeStructTag,
  parseStructTag,
  toHex,
} from "@mysten/sui/utils";

/** Pinned deployment ids the discovery reads (see config.ts). */
export type DbmIds = {
  /** dbm-oracle package (the pinned witness's package), from token-info. */
  oraclePkg: string;
  /** Shared DeepBook-Margin `MarginRegistry`. */
  marginRegistryId: string;
  /** DeepBook-Margin ORIGINAL publish id (dynamic-field key struct types). */
  originalPkg: string;
  /** Pyth state's `price_info` Table id (feed id → PriceInfoObject id). */
  pythPriceInfoTableId: string;
  /** Latest Pyth package id (`price_identifier::PriceIdentifier` key type). */
  pythPkg: string;
};

/** Everything the composer needs for one `dbm_oracle::record{,_no_debt}`. */
export type DbmLeg = {
  oraclePkg: string;
  managerId: string;
  poolId: string;
  /** Canonical base/quote coin types from the manager's type args. */
  baseType: string;
  quoteType: string;
  /** The pool's two `MarginPool`s from its registered `PoolConfig`. */
  baseMarginPoolId: string;
  quoteMarginPoolId: string;
  /** Borrowed side (canonical asset + its `MarginPool`); null ⇒ debt-free
   * (`record_no_debt`). Read fresh on every resolve — debt can change. */
  debt: { asset: string; marginPoolId: string } | null;
  /** Canonical type → Pyth feed id (lower-case hex, no 0x), base + quote. */
  feedIdByType: Record<string, string>;
  /** Feed id → shared `PriceInfoObject` id. */
  priceInfoByFeed: Record<string, string>;
};

// ── Move-JSON tolerant reads (local copies of appraisal.ts's helpers —
// imported here they'd form an import cycle with the composer) ──────────────

function asRecord(v: unknown): Record<string, unknown> | null {
  return typeof v === "object" && v !== null && !Array.isArray(v)
    ? (v as Record<string, unknown>)
    : null;
}

function structFields(v: unknown): Record<string, unknown> | null {
  const r = asRecord(v);
  if (!r) return null;
  return asRecord(r.fields) ?? r;
}

function idString(v: unknown): string | null {
  if (typeof v === "string") return v;
  const f = structFields(v);
  if (!f) return null;
  for (const key of ["bytes", "id"]) {
    const x = f[key];
    if (typeof x === "string") return x;
    const nested = structFields(x)?.id;
    if (typeof nested === "string") return nested;
  }
  return null;
}

function typeNameString(v: unknown): string | null {
  if (typeof v === "string") return v;
  const n = structFields(v)?.name;
  return typeof n === "string" ? n : null;
}

/** A `vector<u8>` Move-JSON value (byte array or base64) → lower-case hex. */
function bytesHex(v: unknown): string | null {
  if (typeof v === "string") return toHex(fromBase64(v)).toLowerCase();
  if (Array.isArray(v) && v.every((b) => typeof b === "number")) {
    return toHex(Uint8Array.from(v as number[])).toLowerCase();
  }
  return null;
}

function u64Value(v: unknown, what: string): bigint {
  if (typeof v === "string" || typeof v === "number") return BigInt(v);
  throw new Error(`unparseable u64 for ${what}`);
}

// ── derived dynamic-field reads ─────────────────────────────────────────────

/** `getObject` on the derived df id of `key` under `parentId`, returning the
 * Field's `value` JSON. Throws with `what` when the field doesn't exist. */
async function fieldValue(
  client: SuiGrpcClient,
  parentId: string,
  keyType: string,
  keyBcs: Uint8Array,
  what: string,
): Promise<unknown> {
  const fieldId = deriveDynamicFieldID(parentId, keyType, keyBcs);
  let json: unknown;
  try {
    const { object } = await client.core.getObject({
      objectId: fieldId,
      include: { json: true },
    });
    json = object.json;
  } catch (err) {
    throw new Error(
      `${what} not found (df ${fieldId} under ${parentId})` +
        (err instanceof Error ? `: ${err.message}` : ""),
    );
  }
  const value = structFields(json)?.value;
  if (value === undefined) throw new Error(`${what}: dynamic field has no value`);
  return value;
}

// ── discovery ───────────────────────────────────────────────────────────────

/** Identity that never changes for a vault's registered account. */
type DbmStatics = Omit<DbmLeg, "debt">;

const staticsCache = new Map<string, DbmStatics>();

async function resolveStatics(
  client: SuiGrpcClient,
  ids: DbmIds,
  vaultId: string,
  account: string,
): Promise<DbmStatics> {
  const cached = staticsCache.get(vaultId);
  if (cached) return cached;

  // Registry outer → inner Versioned → MarginRegistryInner (df key u64 = 1,
  // the margin version the registry was created at).
  const { object: registry } = await client.core.getObject({
    objectId: ids.marginRegistryId,
    include: { json: true },
  });
  const versionedId = idString(structFields(structFields(registry.json)?.inner)?.id);
  if (!versionedId) throw new Error("MarginRegistry has no inner Versioned id");
  const inner = structFields(
    await fieldValue(
      client,
      versionedId,
      "u64",
      bcs.u64().serialize(1).toBytes(),
      "MarginRegistryInner",
    ),
  );
  const managersTableId = idString(structFields(inner?.margin_managers)?.id);
  const poolRegistryTableId = idString(structFields(inner?.pool_registry)?.id);
  if (!managersTableId || !poolRegistryTableId) {
    throw new Error("MarginRegistryInner tables unresolvable");
  }

  // margin_managers[account] → VecSet<ID> → the single manager id.
  const managerSet = await fieldValue(
    client,
    managersTableId,
    "address",
    bcs.Address.serialize(account).toBytes(),
    `MarginManager set for account ${account}`,
  );
  const setContents = structFields(managerSet)?.contents;
  const managerIds = (Array.isArray(setContents) ? setContents : [])
    .map(idString)
    .filter((id): id is string => id !== null);
  if (managerIds.length !== 1) {
    throw new Error(
      `expected exactly one MarginManager for account ${account}, found ${managerIds.length}`,
    );
  }
  const managerId = managerIds[0];

  // Manager object: `MarginManager<Base, Quote>` type args + its pool.
  const { object: manager } = await client.core.getObject({
    objectId: managerId,
    include: { json: true },
  });
  const params = parseStructTag(manager.type).typeParams;
  if (params.length < 2) throw new Error(`unexpected MarginManager type: ${manager.type}`);
  const baseType = normalizeStructTag(params[0]);
  const quoteType = normalizeStructTag(params[1]);
  const poolId = idString(structFields(manager.json)?.deepbook_pool);
  if (!poolId) throw new Error(`MarginManager ${managerId} has no deepbook_pool`);

  // pool_registry[pool id] → PoolConfig → the two MarginPool ids.
  const poolConfig = structFields(
    await fieldValue(
      client,
      poolRegistryTableId,
      "0x2::object::ID",
      bcs.Address.serialize(poolId).toBytes(),
      `PoolConfig for pool ${poolId}`,
    ),
  );
  const baseMarginPoolId = idString(poolConfig?.base_margin_pool_id);
  const quoteMarginPoolId = idString(poolConfig?.quote_margin_pool_id);
  if (!baseMarginPoolId || !quoteMarginPoolId) {
    throw new Error(`PoolConfig for pool ${poolId} missing margin pool ids`);
  }

  // Registry's ConfigKey<PythConfig> df → currencies VecMap → feed ids for
  // base + quote (key struct types resolve against the ORIGINAL package;
  // the fieldless ConfigKey serializes as its bool dummy_field = false).
  const pythConfig = structFields(
    await fieldValue(
      client,
      ids.marginRegistryId,
      `${ids.originalPkg}::margin_registry::ConfigKey<${ids.originalPkg}::oracle::PythConfig>`,
      Uint8Array.of(0),
      "DeepBook-Margin PythConfig",
    ),
  );
  const currencies = structFields(pythConfig?.currencies)?.contents;
  const feedIdByType: Record<string, string> = {};
  for (const entry of Array.isArray(currencies) ? currencies : []) {
    const f = structFields(entry);
    const name = typeNameString(f?.key);
    const feed = bytesHex(structFields(f?.value)?.price_feed_id);
    if (!name || !feed) continue;
    const t = normalizeStructTag(name);
    if (t === baseType || t === quoteType) feedIdByType[t] = feed;
  }
  for (const t of [baseType, quoteType]) {
    if (!feedIdByType[t]) {
      throw new Error(`DeepBook-Margin PythConfig has no feed for ${t}`);
    }
  }

  // Feed id → PriceInfoObject via the pinned Pyth price_info table.
  const priceInfoByFeed: Record<string, string> = {};
  for (const feed of new Set(Object.values(feedIdByType))) {
    const info = idString(
      await fieldValue(
        client,
        ids.pythPriceInfoTableId,
        `${ids.pythPkg}::price_identifier::PriceIdentifier`,
        bcs.vector(bcs.u8()).serialize(Array.from(fromHex(feed))).toBytes(),
        `PriceInfoObject for feed ${feed}`,
      ),
    );
    if (!info) throw new Error(`unparseable PriceInfoObject id for feed ${feed}`);
    priceInfoByFeed[feed] = info;
  }

  const statics: DbmStatics = {
    oraclePkg: ids.oraclePkg,
    managerId,
    poolId,
    baseType,
    quoteType,
    baseMarginPoolId,
    quoteMarginPoolId,
    feedIdByType,
    priceInfoByFeed,
  };
  staticsCache.set(vaultId, statics);
  return statics;
}

/**
 * Resolve the vault account's DBM equity leg. The registry walk (manager,
 * pool, margin pools, feeds, PriceInfoObjects) caches per vault id for the
 * session; the manager's borrowed shares are re-read on every call so the
 * `record` vs `record_no_debt` choice can't go stale mid-session.
 */
export async function resolveDbmLeg(
  client: SuiGrpcClient,
  ids: DbmIds,
  vaultId: string,
  account: string,
): Promise<DbmLeg> {
  const statics = await resolveStatics(client, ids, vaultId, account);
  const { object: manager } = await client.core.getObject({
    objectId: statics.managerId,
    include: { json: true },
  });
  const fields = structFields(manager.json);
  const baseShares = u64Value(fields?.borrowed_base_shares, "borrowed_base_shares");
  const quoteShares = u64Value(fields?.borrowed_quote_shares, "borrowed_quote_shares");
  // Debt side selection mirrors the Rust composer: base wins when nonzero;
  // `calculate_debts` aborts on a wrong pool, so it's chain-checked too.
  const debt =
    baseShares === 0n && quoteShares === 0n
      ? null
      : baseShares > 0n
        ? { asset: statics.baseType, marginPoolId: statics.baseMarginPoolId }
        : { asset: statics.quoteType, marginPoolId: statics.quoteMarginPoolId };
  return { ...statics, debt };
}
