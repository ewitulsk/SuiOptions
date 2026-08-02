// Client-side appraisal composer for curated trading-vault deposits (SO-289).
//
// `vault::deposit` only accepts a COMPLETE `Appraisal` hot potato: every
// non-deposit free balance valued via an oracle attestation, and every
// custodied position valued by its adapter — all in the same PTB. This module
// discovers what the vault holds (chain reads over gRPC + the api-service
// detail endpoint), plans the required Pyth legs, and emits the calls:
//
//   1. `vault::begin_appraisal<Dep>`
//   2. one Hermes accumulator update covering every needed feed
//      (wormhole `parse_and_verify` → pyth `create_authenticated_…` →
//      `update_single_price_feed` per feed → `hot_potato_vector::destroy`)
//   3. `oracle_pyth::attest<Asset, Dep>` per needed asset (attestations are
//      `copy, drop`, so one per asset is reused across every leg)
//   4. `vault::appraise_balance<T>` per non-deposit free balance
//   5. per DeepBook custody: `begin_custody_appraisal` → `value_asset<T>` →
//      `value_pool_locked<B,Q>` → `finalize_custody_appraisal`
//   6. per options position: `appraise_rfq_ticket<E>` /
//      `appraise_call_position<U,S,C>` / `appraise_put_position<U,S,P>`
//
// Planning (async chain reads) is split from composition (sync PTB emission)
// so the screen can pre-flight feasibility and disable the deposit button
// with a reason when e.g. a held asset has no Pyth feed.

import { bcs } from "@mysten/sui/bcs";
import type { SuiGrpcClient } from "@mysten/sui/grpc";
import {
  Transaction,
  coinWithBalance,
  type TransactionArgument,
  type TransactionResult,
} from "@mysten/sui/transactions";
import {
  deriveDynamicFieldID,
  fromBase64,
  fromHex,
  normalizeStructTag,
  parseStructTag,
} from "@mysten/sui/utils";

import { fetchBuckets, optionCoinType, seriesOptionType } from "../api/client";
import type { OracleDescriptor } from "../api/oracleDescriptor";
import { HERMES_BASE } from "../api/pyth";
import { tokenForCoinType, type TradingVaultDetail } from "../api/tradingVaults";
import {
  asRecord,
  canon,
  classifyVaultPositions,
  idString,
  shortType,
  structFields,
  typeNameString,
  vecSetItems,
  type CustodyPlan,
  type OptionPositionPlan,
  type RfqTicketPlan,
} from "../api/vaultHoldings";
import {
  DEEP_COIN_TYPE,
  DEEPBOOK_ADAPTER_PACKAGE_ID,
  ENV,
  EQUITY_ORACLE_PACKAGE_ID,
  EQUITY_ORACLE_PUBLISH_DIGEST,
  OPTIONS_ADAPTER_PACKAGE_ID,
  ORACLE_PYTH_PACKAGE_ID,
  PYTH_PRICE_INFO_TABLE_IDS,
  TRADING_VAULT_OBJECTS,
  TRADING_VAULT_PACKAGE_ID,
} from "../config";

const CLOCK_ID = "0x6";

// ═══════════════════════════ Pyth deployment ═══════════════════════════

// Pyth + Wormhole handles per network. Package ids are the latest (upgraded)
// packages the entry calls target; state ids never change. Mirrors the
// backend keeper's `[pyth]` config (services/keeper/config/config.*.toml) —
// both staging and prod run on Sui testnet against the beta feed set.
type PythHandles = {
  pythPackage: string;
  wormholePackage: string;
  pythStateId: string;
  wormholeStateId: string;
  /** Base update fee per feed in MIST, split from gas. */
  updateFeeMist: bigint;
};

const PYTH_HANDLES: Partial<Record<string, PythHandles>> = {
  testnet: {
    pythPackage: "0xabf837e98c26087cba0883c0a7a28326b1fa3c5e1e2c5abdb486f9e8f594c837",
    wormholePackage: "0xf47329f4344f3bf0f8e436e2f7b485466cff300f12a166563995d3888c296a94",
    pythStateId: "0x243759059f4c3111179da5878c12f68d612c21a8d54d85edc86164bb18be1c7c",
    wormholeStateId: "0x31358d198147da50db32eda2562951d53973a0c0ad5ed738e9b17d88b213d790",
    updateFeeMist: 1n,
  },
};

// ═══════════════════════════════ plan types ═══════════════════════════════

// Position-classification types (`CustodyPlan`, `RfqTicketPlan`,
// `OptionPositionPlan`, `PoolLegPlan`) live in `api/vaultHoldings.ts`,
// shared with the vault-detail positions UI (SO-303).

/** One option-coin type the vault holds (custody balance or pool leg),
 * priced via `options_oracle::attest_call/put` from its bucket. */
export type OptionLegPlan = {
  /** Canonical option coin type. */
  coinType: string;
  bucketId: string;
  underlying: string;
  settlement: string;
  isPut: boolean;
};

/** A held option coin custodied as a position (vault_mm writer flow). */
export type OptionCoinPlan = OptionLegPlan & { positionId: string };

/** The external-account equity leg (SO-299): a vault with a FUNDED external
 * account (exposure > 0) marks every appraisal `external_pending`, so
 * consumption needs the pinned oracle's `equity_oracle::record` leg from the
 * keeper-posted `EquityBook`. An unfunded account marks nothing and takes NO
 * leg (SO-310). */
export type ExternalEquityPlan = {
  kind: "equityBook";
  /** equity-oracle package id (the pinned witness's package). */
  oraclePkg: string;
  /** Shared `EquityBook` object id. */
  bookId: string;
};

export type AppraisalPlan = {
  vaultId: string;
  /** Canonical deposit coin type. */
  depositType: string;
  /** Non-deposit free-balance types needing `appraise_balance` (canonical). */
  freeBalanceTypes: string[];
  custodies: CustodyPlan[];
  rfqTickets: RfqTicketPlan[];
  optionPositions: OptionPositionPlan[];
  /** Option-coin types priced via the options oracle (not Pyth). */
  optionLegs: OptionLegPlan[];
  /** Held option coins custodied as positions. */
  optionCoins: OptionCoinPlan[];
  /** Non-deposit assets needing one `attest` each (canonical). */
  attestTypes: string[];
  /** Canonical coin type → Pyth feed id (lower-case hex, no 0x). Includes the
   * deposit asset whenever any attestation is needed (it's the quote leg). */
  feedIdByType: Record<string, string>;
  /** Feed id → shared `PriceInfoObject` id. */
  priceInfoByFeed: Record<string, string>;
  /** Canonical DEEP type when locked-DEEP legs can be attested, else null. */
  deepType: string | null;
  /** Non-null when the vault has a funded external account — its equity leg
   * is mandatory for a complete appraisal. */
  externalEquity: ExternalEquityPlan | null;
  /** Non-null when the vault pins the keeper-posted `EquityOracle`, is still
   * unfunded, and its `EquityBook` entry does not exist yet (SO-310): the
   * first release must create it, or every later appraisal aborts
   * E_NOT_SEEDED. See `buildReleaseExternalTx`. */
  externalInit: { oraclePkg: string; bookId: string } | null;
};

// Move-JSON tolerant read helpers (`structFields`, `idString`, …) moved to
// `api/vaultHoldings.ts` alongside the shared position classification.

// ═══════════════════════════ PriceInfoObject cache ═══════════════════════════

// Pyth's state maps feed id → `PriceInfoObject` through a
// `Table<PriceIdentifier, ID>` hung off the state as the dynamic field named
// `b"price_info"` (same lookup as the backend keeper and pyth-sui-js). A
// `PriceInfoObject` is never re-created for a feed, so both the table handle
// and per-feed ids cache for the session.
let priceInfoTable: { tableId: string; keyType: string } | null = null;
const priceInfoCache = new Map<string, string>();

async function resolvePriceInfoTable(
  client: SuiGrpcClient,
  _handles: PythHandles,
): Promise<{ tableId: string; keyType: string }> {
  if (priceInfoTable) return priceInfoTable;
  // The table id is pinned per network: `price_info` hangs off the Pyth
  // state as a dynamic OBJECT field, whose wrapped-key derivation the
  // plain getDynamicField cannot perform (it derives a nonexistent id) —
  // and some RPC providers serve no dynamic-field index at all. A plain
  // object read of the pinned table gives its key type.
  const tableId = PYTH_PRICE_INFO_TABLE_IDS[ENV];
  if (!tableId) {
    throw new Error(`No Pyth price_info table pinned for network "${ENV}"`);
  }
  const { object } = await client.core.getObject({ objectId: tableId, include: {} });
  const tag = parseStructTag(object.type);
  const keyParam = tag.typeParams[0];
  const keyType =
    typeof keyParam === "string" ? keyParam : normalizeStructTag(keyParam);
  if (!keyType.endsWith("::price_identifier::PriceIdentifier")) {
    throw new Error(`unexpected Pyth price_info table key type: ${keyType}`);
  }
  priceInfoTable = { tableId, keyType };
  return priceInfoTable;
}

async function resolvePriceInfoObjectId(
  client: SuiGrpcClient,
  handles: PythHandles,
  feedId: string,
): Promise<string> {
  const cached = priceInfoCache.get(feedId);
  if (cached) return cached;
  const table = await resolvePriceInfoTable(client, handles);
  const keyBcs = bcs
    .vector(bcs.u8())
    .serialize(Array.from(fromHex(feedId)))
    .toBytes();
  // Derive the Table entry's field id client-side + plain object read
  // (no dynamic-field index API on the configured RPC).
  const fieldId = deriveDynamicFieldID(table.tableId, table.keyType, keyBcs);
  let objectId: string;
  try {
    const { object } = await client.core.getObject({
      objectId: fieldId,
      include: { json: true },
    });
    const value = (object.json as { value?: unknown } | null)?.value;
    if (typeof value !== "string") {
      throw new Error(`unparseable price_info entry ${fieldId}`);
    }
    objectId = value;
  } catch (err) {
    throw new Error(
      `Pyth feed ${feedId.slice(0, 8)}… has no PriceInfoObject on this network` +
        (err instanceof Error ? ` (${err.message})` : ""),
    );
  }
  priceInfoCache.set(feedId, objectId);
  return objectId;
}

// ═══════════════════════════ EquityBook cache ═══════════════════════════

// The equity-oracle's shared `EquityBook` is created once in its `init`;
// resolve it from the package's publish effects (token-info doesn't serve
// the id — same fallback pattern as `useVaultProtocolConfigId`) and cache
// for the session.
let equityBookCache: string | null = null;

async function resolveEquityBookId(client: SuiGrpcClient): Promise<string> {
  if (equityBookCache) return equityBookCache;
  if (!EQUITY_ORACLE_PUBLISH_DIGEST) {
    throw new Error("equity-oracle publish digest unavailable — cannot resolve the EquityBook");
  }
  const res = await client.core.getTransaction({
    digest: EQUITY_ORACLE_PUBLISH_DIGEST,
    include: { effects: true, objectTypes: true },
  });
  const txn = res.Transaction ?? res.FailedTransaction;
  const types = txn.objectTypes ?? {};
  for (const change of txn.effects?.changedObjects ?? []) {
    if (change.idOperation !== "Created") continue;
    if (types[change.objectId]?.endsWith("::equity_oracle::EquityBook")) {
      equityBookCache = change.objectId;
      return change.objectId;
    }
  }
  throw new Error("EquityBook not found in the equity-oracle publish transaction");
}

/** Whether the book already holds this vault's entry (SO-310). The entries
 * live in a `Table<ID, EquityEntry>`, whose per-key field id derives
 * client-side — same posture as the Pyth price_info read above. */
async function equityBookHasEntry(
  client: SuiGrpcClient,
  bookId: string,
  vaultId: string,
): Promise<boolean> {
  const { object } = await client.core.getObject({ objectId: bookId, include: { json: true } });
  const tableId = idString(structFields(object.json)?.entries);
  if (!tableId) throw new Error("unparseable EquityBook entries table");
  const fieldId = deriveDynamicFieldID(
    tableId,
    "0x2::object::ID",
    bcs.Address.serialize(vaultId).toBytes(),
  );
  try {
    await client.core.getObject({ objectId: fieldId, include: {} });
    return true;
  } catch {
    return false;
  }
}

// ═══════════════════════════════ planning ═══════════════════════════════

function feedIdFor(coinType: string): string | null {
  const feed = tokenForCoinType(coinType)?.pythFeedId ?? null;
  if (!feed) return null;
  return (feed.startsWith("0x") ? feed.slice(2) : feed).toLowerCase();
}

/** Option-coin type → bucket identity, from the api-service bucket catalog
 * (expired series included — their coins still need zero/dust marks). */
async function optionBucketCatalog(): Promise<Map<string, Omit<OptionLegPlan, "coinType">>> {
  const series = await fetchBuckets();
  const map = new Map<string, Omit<OptionLegPlan, "coinType">>();
  for (const s of series) {
    for (const b of s.buckets) {
      map.set(canon(optionCoinType(b)), {
        bucketId: b.bucket_id,
        underlying: canon(s.asset_coin_type),
        settlement: canon(s.settlement_coin_type),
        isPut: seriesOptionType(s) === "put",
      });
    }
  }
  return map;
}

/**
 * Discover the vault's holdings and pre-resolve everything the composer
 * needs. Throws with a human-readable reason when a deposit cannot be
 * composed (missing adapter deployment ids, an asset without a Pyth feed,
 * an unresolvable PriceInfoObject, …) — screens surface the message as the
 * disabled-deposit reason.
 */
export async function planAppraisal(
  client: SuiGrpcClient,
  vault: TradingVaultDetail,
): Promise<AppraisalPlan> {
  if (!TRADING_VAULT_PACKAGE_ID) {
    throw new Error("No trading-vault deployment on this network");
  }
  const depositType = canon(vault.depositAsset);

  // 1. The vault object's `asset_types` VecSet<TypeName> — every type with a
  //    non-zero free balance.
  const vaultObj = await client.core.getObject({
    objectId: vault.vaultId,
    include: { json: true },
  });
  const json = vaultObj.object.json;
  const freeTypes = vecSetItems(structFields(json)?.asset_types ?? asRecord(json)?.asset_types)
    .map(typeNameString)
    .filter((t): t is string => t !== null)
    .map(canon);
  const freeBalanceTypes = freeTypes.filter((t) => t !== depositType);

  // 1b. External account (SO-299): a vault with OPEN EXPOSURE marks every
  //     appraisal `external_pending` at begin_appraisal, and consumption
  //     aborts without the pinned oracle's `record_external_equity` leg.
  //     Plan the keeper-posted equity_oracle leg and refuse with a clear
  //     reason (mirroring the Rust composer) for any other witness. A
  //     registered-but-unfunded account (exposure == 0,
  //     SO-310) marks nothing, so it takes NO leg — recording equity for it
  //     would abort. It needs its EquityBook entry created instead, before
  //     the first release opens exposure (`externalInit`).
  let externalEquity: ExternalEquityPlan | null = null;
  let externalInit: AppraisalPlan["externalInit"] = null;
  const extRaw = structFields(json)?.external ?? asRecord(json)?.external;
  const ext = Array.isArray(extRaw) ? extRaw[0] : extRaw;
  if (ext != null) {
    const witness = typeNameString(structFields(ext)?.equity_oracle);
    if (!witness) throw new Error("vault external account has no pinned equity oracle");
    const witnessCanon = canon(witness);
    const exposureRaw = structFields(ext)?.exposure;
    const exposure =
      typeof exposureRaw === "string" || typeof exposureRaw === "number"
        ? BigInt(exposureRaw)
        : 0n;
    if (exposure === 0n) {
      if (
        EQUITY_ORACLE_PACKAGE_ID &&
        witnessCanon === canon(`${EQUITY_ORACLE_PACKAGE_ID}::equity_oracle::EquityOracle`)
      ) {
        const bookId = await resolveEquityBookId(client);
        if (!(await equityBookHasEntry(client, bookId, vault.vaultId))) {
          externalInit = { oraclePkg: EQUITY_ORACLE_PACKAGE_ID, bookId };
        }
      }
    } else if (
      EQUITY_ORACLE_PACKAGE_ID &&
      witnessCanon === canon(`${EQUITY_ORACLE_PACKAGE_ID}::equity_oracle::EquityOracle`)
    ) {
      externalEquity = {
        kind: "equityBook",
        oraclePkg: EQUITY_ORACLE_PACKAGE_ID,
        bookId: await resolveEquityBookId(client),
      };
    } else {
      throw new Error(`unsupported external equity oracle ${shortType(witnessCanon)}`);
    }
  }

  // 2. Classify every active custodied position via its object type (shared
  //    with the positions UI; strict mode throws the human-readable reason).
  const active = vault.positions.filter((p) => p.active);
  const { custodies, rfqTickets, optionPositions, coinPositions } =
    await classifyVaultPositions(client, active);

  // 3. Every non-deposit asset needing a price: free balances ∪ custody
  //    assets ∪ pool locked legs ∪ option escrow/underlying/settlement.
  const needed = new Set<string>(freeBalanceTypes);
  const deepCanon = DEEP_COIN_TYPE ? canon(DEEP_COIN_TYPE) : null;
  let anyPools = false;
  for (const c of custodies) {
    for (const a of c.assets) if (a !== depositType) needed.add(a);
    for (const p of c.pools) {
      anyPools = true;
      if (p.baseType !== depositType) needed.add(p.baseType);
      if (p.quoteType !== depositType) needed.add(p.quoteType);
    }
  }
  for (const t of rfqTickets) if (t.escrowType !== depositType) needed.add(t.escrowType);
  for (const p of optionPositions) {
    const [u, s] = p.bucketTypeArgs;
    if (u !== depositType) needed.add(u);
    if (s !== depositType) needed.add(s);
  }

  // Locked DEEP in active pools: attestable only when DEEP has a served
  // feed. Without one the leg gets `none` — fine while locked DEEP is zero.
  let deepType: string | null = null;
  if (anyPools && deepCanon && deepCanon !== depositType && feedIdFor(deepCanon)) {
    deepType = deepCanon;
    needed.add(deepCanon);
  }

  // Option-coin types price via `options_oracle` from their bucket, not
  // Pyth: swap each mapped type out of the Pyth set and pull its bucket's
  // underlying + settlement legs in. Held coin positions contribute their
  // legs the same way.
  const optionLegs: OptionLegPlan[] = [];
  const optionCoins: OptionCoinPlan[] = [];
  if (needed.size > 0 || coinPositions.length > 0) {
    const catalog = await optionBucketCatalog();
    for (const t of [...needed]) {
      const leg = catalog.get(t);
      if (!leg) continue;
      needed.delete(t);
      optionLegs.push({ ...leg, coinType: t });
      if (leg.underlying !== depositType) needed.add(leg.underlying);
      if (leg.settlement !== depositType) needed.add(leg.settlement);
    }
    for (const cp of coinPositions) {
      const leg = catalog.get(cp.coinType);
      if (!leg) {
        throw new Error(
          `Held option coin ${shortType(cp.coinType)} has no bucket in the catalog`,
        );
      }
      optionCoins.push({ ...leg, coinType: cp.coinType, positionId: cp.positionId });
      if (leg.underlying !== depositType) needed.add(leg.underlying);
      if (leg.settlement !== depositType) needed.add(leg.settlement);
    }
    if ((optionLegs.length > 0 || optionCoins.length > 0) && !OPTIONS_ADAPTER_PACKAGE_ID) {
      throw new Error("options-adapter package not deployed on this network");
    }
  }

  const feedFor = (t: string): string | null => feedIdFor(t);

  // Optimistic legs (mirrors the Rust composer): types with no served feed —
  // e.g. option coins tracked by a custody from placing orders — get an
  // `option::none` leg instead of blocking the plan. The chain aborts the
  // appraisal only if such an asset's actual balance is nonzero, so this is
  // exactly as strict as the contract. Free-balance assets still hard-error:
  // `appraise_balance` needs a real attestation.
  const attestTypes: string[] = [];
  for (const t of [...needed].sort()) {
    if (feedFor(t)) attestTypes.push(t);
    else if (freeBalanceTypes.includes(t)) {
      const token = tokenForCoinType(t);
      throw new Error(`No Pyth feed for held asset ${token?.ticker ?? shortType(t)}`);
    }
    // else: feedless custody/pool/option leg — composed as `option::none`.
  }

  // 4. Feed ids + PriceInfoObjects (deposit feed included — `attest` crosses
  //    the asset feed with the deposit feed).
  const feedIdByType: Record<string, string> = {};
  if (attestTypes.length > 0) {
    if (!ORACLE_PYTH_PACKAGE_ID) {
      throw new Error("oracle-pyth package not deployed on this network");
    }
    if (!TRADING_VAULT_OBJECTS) {
      throw new Error("trading-vault governance objects not served by token-info");
    }
    const handles = PYTH_HANDLES[ENV];
    if (!handles) {
      throw new Error(`No Pyth deployment configured for network "${ENV}"`);
    }
    if (custodies.length > 0 && !DEEPBOOK_ADAPTER_PACKAGE_ID) {
      throw new Error("deepbook-adapter package not deployed on this network");
    }
    if ((rfqTickets.length > 0 || optionPositions.length > 0) && !OPTIONS_ADAPTER_PACKAGE_ID) {
      throw new Error("options-adapter package not deployed on this network");
    }
    for (const t of [...attestTypes, depositType]) {
      const feed = feedFor(t);
      if (!feed) {
        // Only reachable for the deposit asset — attestTypes are pre-filtered.
        const token = tokenForCoinType(t);
        throw new Error(`No Pyth feed for deposit asset ${token?.ticker ?? shortType(t)}`);
      }
      feedIdByType[t] = feed;
    }
    const priceInfoByFeed: Record<string, string> = {};
    for (const feed of new Set(Object.values(feedIdByType))) {
      priceInfoByFeed[feed] = await resolvePriceInfoObjectId(client, handles, feed);
    }
    return {
      vaultId: vault.vaultId,
      depositType,
      freeBalanceTypes,
      custodies,
      rfqTickets,
      optionPositions,
      optionLegs,
      optionCoins,
      attestTypes,
      feedIdByType,
      priceInfoByFeed,
      deepType,
      externalEquity,
      externalInit,
    };
  }

  // Custodies/positions can exist with zero attestations needed (everything
  // deposit-denominated) — adapter package ids are still required then.
  if (custodies.length > 0 && !DEEPBOOK_ADAPTER_PACKAGE_ID) {
    throw new Error("deepbook-adapter package not deployed on this network");
  }
  if ((rfqTickets.length > 0 || optionPositions.length > 0) && !OPTIONS_ADAPTER_PACKAGE_ID) {
    throw new Error("options-adapter package not deployed on this network");
  }
  return {
    vaultId: vault.vaultId,
    depositType,
    freeBalanceTypes,
    custodies,
    rfqTickets,
    optionPositions,
    optionLegs,
    optionCoins,
    attestTypes,
    feedIdByType,
    priceInfoByFeed: {},
    deepType,
    externalEquity,
    externalInit,
  };
}

// ═══════════════════════════ Hermes accumulator ═══════════════════════════

/**
 * One Hermes accumulator update covering every feed in the plan, fetched at
 * submit time so the on-chain staleness gates see fresh publish times.
 */
export async function fetchHermesAccumulatorUpdate(feedIds: string[]): Promise<Uint8Array> {
  const qs = feedIds.map((id) => `ids[]=0x${id}`).join("&");
  const res = await fetch(`${HERMES_BASE}/v2/updates/price/latest?${qs}&encoding=base64`);
  if (!res.ok) {
    throw new Error(`Hermes price update failed: ${res.status} ${res.statusText}`);
  }
  const body = (await res.json()) as { binary?: { data?: string[] } };
  const data = body.binary?.data;
  if (!data || data.length === 0) throw new Error("Hermes returned no update data");
  if (data.length > 1) {
    // Hermes packs all requested feeds into one accumulator blob today; a
    // multi-chunk response would need one update prefix per chunk.
    throw new Error(`Hermes returned ${data.length} update chunks; expected 1`);
  }
  return fromBase64(data[0]);
}

/** Accumulator-update magic: `"PNAU"`. */
const ACCUMULATOR_MAGIC = [0x50, 0x4e, 0x41, 0x55];
const PROOF_TYPE_WORMHOLE_MERKLE = 0;

/**
 * Pull the embedded Wormhole VAA out of a Hermes accumulator message. Wire
 * layout (port of `sui-tx::tx::pyth_update::extract_vaa_from_accumulator`):
 *
 *   magic "PNAU" | major u8 | minor u8 | trailer_len u8 | trailer …
 *   | proof_type u8 (0 = wormhole merkle) | vaa_len u16be | vaa …
 */
export function extractVaaFromAccumulator(update: Uint8Array): Uint8Array {
  const need = (n: number) => {
    if (update.length < n) {
      throw new Error(`accumulator update truncated: need ${n} bytes, have ${update.length}`);
    }
  };
  need(8);
  if (!ACCUMULATOR_MAGIC.every((b, i) => update[i] === b)) {
    throw new Error("not an accumulator update (bad magic)");
  }
  if (update[4] !== 1) throw new Error(`unsupported accumulator major version ${update[4]}`);
  const trailerLen = update[6];
  let off = 7 + trailerLen;
  need(off + 3);
  if (update[off] !== PROOF_TYPE_WORMHOLE_MERKLE) {
    throw new Error(`unsupported accumulator proof type ${update[off]}`);
  }
  off += 1;
  const vaaLen = (update[off] << 8) | update[off + 1];
  off += 2;
  need(off + vaaLen);
  return update.slice(off, off + vaaLen);
}

// ══════════════════════════════ composition ══════════════════════════════

export type ComposeContext = {
  /** Shared `VaultProtocolConfig` object id. */
  protocolConfigId: string;
  /** Hermes accumulator update; required iff `plan.attestTypes` is non-empty. */
  accumulatorUpdate: Uint8Array | null;
};

function requireId(id: string | undefined, what: string): string {
  if (!id) throw new Error(`${what} unavailable — cannot compose appraisal`);
  return id;
}

/**
 * Emit the full appraisal-leg sequence into `tx` and return the `Appraisal`
 * argument for `vault::deposit`. Synchronous — all discovery lives in
 * `planAppraisal`, all network fetches in `fetchHermesAccumulatorUpdate`.
 */
export function composeAppraisal(
  tx: Transaction,
  plan: AppraisalPlan,
  ctx: ComposeContext,
  /**
   * The live oracle descriptor (SO-335). Supplied, the attest legs
   * target whichever adapter oracle-service says is live; omitted, they
   * fall back to the compiled Pyth ids. Optional so existing callers
   * keep working — new ones should pass it.
   */
  oracle?: OracleDescriptor,
): TransactionResult {
  const vaultPkg = requireId(TRADING_VAULT_PACKAGE_ID, "trading-vault package");
  const vault = tx.object(plan.vaultId);
  const cfg = tx.object(ctx.protocolConfigId);
  const clock = tx.object(CLOCK_ID);

  // 1. begin_appraisal<Dep>
  const appraisal = tx.moveCall({
    target: `${vaultPkg}::vault::begin_appraisal`,
    typeArguments: [plan.depositType],
    arguments: [vault],
  });

  // 1b. External-account equity leg (SO-299): mandatory whenever the vault
  // has a FUNDED external account. The chain gates the EquityBook entry's
  // age.
  if (plan.externalEquity) {
    const gov = TRADING_VAULT_OBJECTS;
    if (!gov) throw new Error("trading-vault governance objects unavailable");
    tx.moveCall({
      target: `${plan.externalEquity.oraclePkg}::equity_oracle::record`,
      arguments: [
        vault,
        tx.object(plan.externalEquity.bookId),
        tx.object(gov.oracleRegistryId),
        appraisal,
        clock,
      ],
    });
  }

  // 2. Pyth update prefix + 3. one attestation per asset.
  const attestations = new Map<string, TransactionResult>();
  if (plan.attestTypes.length > 0) {
    // SO-335: the adapter is whatever oracle-service says is live, not a
    // constant baked into this bundle. `oracle` is passed in by the
    // caller (from `useOracleDescriptor`); falling back to the compiled
    // Pyth ids keeps older callers working during the migration.
    const adapter = oracle?.adapter;
    const oraclePkg = adapter
      ? adapter.adapter_package_id
      : requireId(ORACLE_PYTH_PACKAGE_ID, "oracle-pyth package");
    const attestModule = oracle?.adapter_module ?? "oracle_pyth";
    const gov = TRADING_VAULT_OBJECTS;
    if (!gov) throw new Error("trading-vault governance objects unavailable");
    const handles = PYTH_HANDLES[ENV];
    if (!handles) throw new Error(`No Pyth deployment configured for network "${ENV}"`);
    if (!ctx.accumulatorUpdate) throw new Error("missing Hermes accumulator update");

    const vaa = extractVaaFromAccumulator(ctx.accumulatorUpdate);
    const wormholeState = tx.object(handles.wormholeStateId);
    const pythState = tx.object(handles.pythStateId);
    const verifiedVaa = tx.moveCall({
      target: `${handles.wormholePackage}::vaa::parse_and_verify`,
      arguments: [
        wormholeState,
        tx.pure(bcs.vector(bcs.u8()).serialize(Array.from(vaa))),
        clock,
      ],
    });
    let potato = tx.moveCall({
      target: `${handles.pythPackage}::pyth::create_authenticated_price_infos_using_accumulator`,
      arguments: [
        pythState,
        tx.pure(bcs.vector(bcs.u8()).serialize(Array.from(ctx.accumulatorUpdate))),
        verifiedVaa,
        clock,
      ],
    });
    const feeds = [...new Set(Object.values(plan.feedIdByType))];
    for (const feed of feeds) {
      const info = plan.priceInfoByFeed[feed];
      if (!info) throw new Error(`no PriceInfoObject resolved for feed ${feed}`);
      const [fee] = tx.splitCoins(tx.gas, [tx.pure.u64(handles.updateFeeMist)]);
      potato = tx.moveCall({
        target: `${handles.pythPackage}::pyth::update_single_price_feed`,
        arguments: [pythState, potato, tx.object(info), fee, clock],
      });
    }
    tx.moveCall({
      target: `${handles.pythPackage}::hot_potato_vector::destroy`,
      typeArguments: [`${handles.pythPackage}::price_info::PriceInfo`],
      arguments: [potato],
    });

    // Attestations: `attest<Asset, Dep>` crosses the asset feed with the
    // deposit feed. `PriceAttestation` is `copy, drop`, so one result per
    // asset is reused across every leg below.
    const feedReg = tx.object(adapter ? adapter.feed_registry_id : gov.pythFeedRegistryId);
    const oracleReg = tx.object(adapter ? adapter.oracle_registry_id : gov.oracleRegistryId);
    const depositInfo = tx.object(plan.priceInfoByFeed[plan.feedIdByType[plan.depositType]]);
    for (const asset of plan.attestTypes) {
      const info = tx.object(plan.priceInfoByFeed[plan.feedIdByType[asset]]);
      attestations.set(
        asset,
        tx.moveCall({
          target: `${oraclePkg}::${attestModule}::attest`,
          typeArguments: [asset, plan.depositType],
          arguments: [feedReg, oracleReg, info, depositInfo, clock],
        }),
      );
    }
  }

  const attestationType = `${vaultPkg}::price::PriceAttestation`;
  const someAtt = (asset: string): TransactionArgument => {
    const att = attestations.get(asset);
    if (!att) throw new Error(`no attestation composed for ${shortType(asset)}`);
    return tx.moveCall({
      target: "0x1::option::some",
      typeArguments: [attestationType],
      arguments: [att],
    });
  };
  const noneAtt = (): TransactionArgument =>
    tx.moveCall({
      target: "0x1::option::none",
      typeArguments: [attestationType],
      arguments: [],
    });
  /** `Option<PriceAttestation>`: some for priced non-deposit assets, none
   * for the deposit asset (adapters self-value it 1:1) or unpriced DEEP. */
  const optAtt = (asset: string): TransactionArgument =>
    asset === plan.depositType || !attestations.has(asset) ? noneAtt() : someAtt(asset);

  // 3b. Option-coin attestations: intrinsic via the options oracle, fed by
  // the Pyth legs above (`none` for deposit-asset legs or once expired; a
  // live bucket with a genuinely missing leg aborts on-chain).
  if (plan.optionLegs.length > 0) {
    const oa = requireId(OPTIONS_ADAPTER_PACKAGE_ID, "options-adapter package");
    const gov = TRADING_VAULT_OBJECTS;
    if (!gov) throw new Error("trading-vault governance objects unavailable");
    if (!gov.volBookId) throw new Error("vol book unavailable for option-coin legs");
    const oracleReg = tx.object(gov.oracleRegistryId);
    const volBook = tx.object(gov.volBookId);
    for (const leg of plan.optionLegs) {
      const att = tx.moveCall({
        target: `${oa}::options_oracle::${leg.isPut ? "attest_put" : "attest_call"}`,
        typeArguments: [leg.underlying, leg.settlement, leg.coinType, plan.depositType],
        arguments: [oracleReg, tx.object(leg.bucketId), volBook, optAtt(leg.underlying), optAtt(leg.settlement), clock],
      });
      attestations.set(leg.coinType, att);
    }
  }

  // 4. Non-deposit free balances.
  for (const t of plan.freeBalanceTypes) {
    const att = attestations.get(t);
    if (!att) throw new Error(`no attestation composed for ${shortType(t)}`);
    tx.moveCall({
      target: `${vaultPkg}::vault::appraise_balance`,
      typeArguments: [t],
      arguments: [vault, cfg, appraisal, att, clock],
    });
  }

  // 5. DeepBook custodies.
  if (plan.custodies.length > 0) {
    const adapterPkg = requireId(DEEPBOOK_ADAPTER_PACKAGE_ID, "deepbook-adapter package");
    for (const custody of plan.custodies) {
      const ca = tx.moveCall({
        target: `${adapterPkg}::deepbook_adapter::begin_custody_appraisal`,
        arguments: [vault, tx.pure.id(custody.custodyId)],
      });
      for (const asset of custody.assets) {
        tx.moveCall({
          target: `${adapterPkg}::deepbook_adapter::value_asset`,
          typeArguments: [asset],
          arguments: [vault, cfg, ca, optAtt(asset), clock],
        });
      }
      for (const pool of custody.pools) {
        tx.moveCall({
          target: `${adapterPkg}::deepbook_adapter::value_pool_locked`,
          typeArguments: [pool.baseType, pool.quoteType],
          arguments: [
            vault,
            cfg,
            ca,
            tx.object(pool.poolId),
            optAtt(pool.baseType),
            optAtt(pool.quoteType),
            plan.deepType ? someAtt(plan.deepType) : noneAtt(),
            clock,
          ],
        });
      }
      tx.moveCall({
        target: `${adapterPkg}::deepbook_adapter::finalize_custody_appraisal`,
        arguments: [vault, appraisal, ca],
      });
    }
  }

  // 6. Options positions.
  if (plan.rfqTickets.length > 0 || plan.optionPositions.length > 0) {
    const adapterPkg = requireId(OPTIONS_ADAPTER_PACKAGE_ID, "options-adapter package");
    for (const ticket of plan.rfqTickets) {
      tx.moveCall({
        target: `${adapterPkg}::options_adapter::appraise_rfq_ticket`,
        typeArguments: [ticket.escrowType],
        arguments: [vault, cfg, appraisal, tx.pure.id(ticket.ticketId), optAtt(ticket.escrowType), clock],
      });
    }
    for (const pos of plan.optionPositions) {
      const [underlying, settlement] = pos.bucketTypeArgs;
      const fn = pos.isPut ? "appraise_put_position" : "appraise_call_position";
      // The appraisal witness must match the position's adapter tag.
      const target = pos.viaVaultMm
        ? `${vaultPkg}::vault_mm::${fn}`
        : `${adapterPkg}::options_adapter::${fn}`;
      tx.moveCall({
        target,
        typeArguments: pos.bucketTypeArgs,
        arguments: [
          vault,
          cfg,
          appraisal,
          tx.object(pos.bucketId),
          tx.pure.id(pos.positionId),
          optAtt(underlying),
          optAtt(settlement),
          clock,
        ],
      });
    }
  }

  // 6b. Held option coins (vault_mm writer-flow custody).
  for (const oc of plan.optionCoins) {
    tx.moveCall({
      target: `${vaultPkg}::vault_mm::${oc.isPut ? "appraise_put_coin" : "appraise_call_coin"}`,
      typeArguments: [oc.underlying, oc.settlement, oc.coinType],
      arguments: [
        vault,
        cfg,
        appraisal,
        tx.object(oc.bucketId),
        tx.pure.id(oc.positionId),
        optAtt(oc.underlying),
        optAtt(oc.settlement),
        clock,
      ],
    });
  }

  return appraisal;
}

// ═══════════════════════════ deposit convenience ═══════════════════════════

export type AppraisedDepositParams = {
  plan: AppraisalPlan;
  /** Shared `VaultProtocolConfig` object id. */
  protocolConfigId: string;
  /** Deposit amount in smallest units. */
  amountRaw: bigint;
};

/**
 * The full deposit PTB: appraisal legs (with a fresh Hermes update when any
 * attestation is needed) piped into `vault::deposit<T>`. For a vault holding
 * nothing but its deposit asset this degenerates to the same two-call PTB
 * `buildTradingVaultDepositTx` emits.
 */
export async function buildAppraisedDepositTx(p: AppraisedDepositParams): Promise<Transaction> {
  const vaultPkg = requireId(TRADING_VAULT_PACKAGE_ID, "trading-vault package");
  const accumulatorUpdate =
    p.plan.attestTypes.length > 0
      ? await fetchHermesAccumulatorUpdate([
          ...new Set(Object.values(p.plan.feedIdByType)),
        ])
      : null;

  const tx = new Transaction();
  const appraisal = composeAppraisal(tx, p.plan, {
    protocolConfigId: p.protocolConfigId,
    accumulatorUpdate,
  });
  const funds = tx.add(coinWithBalance({ balance: p.amountRaw, type: p.plan.depositType }));
  tx.moveCall({
    target: `${vaultPkg}::vault::deposit`,
    typeArguments: [p.plan.depositType],
    arguments: [
      tx.object(p.plan.vaultId),
      tx.object(p.protocolConfigId),
      appraisal,
      funds,
      tx.object(CLOCK_ID),
    ],
  });
  return tx;
}

// ═══════════════════════════ external release ═══════════════════════════

export type ReleaseExternalParams = {
  plan: AppraisalPlan;
  /** Shared `VaultProtocolConfig` object id. */
  protocolConfigId: string;
  /** The curator's owned `CuratorCap` object id. */
  curatorCapId: string;
  /** Release amount in deposit-asset smallest units. */
  amountRaw: bigint;
};

/**
 * `vault::release_external<T>` (SO-299): the same appraisal-leg sequence as
 * a deposit piped into the curator-gated budgeted release — the chain binds
 * the external budget and daily release window against the NAV this
 * appraisal snapshots, so no client-side limit enforcement is attempted.
 *
 * The FIRST release also creates the vault's `EquityBook` entry (SO-310,
 * `plan.externalInit`): `equity_oracle::init_entry` is permissionless and
 * only legal while exposure is still zero, so it is prepended INTO this PTB
 * — atomically before the release opens exposure. Losing the race to the
 * keeper's own opportunistic init aborts the release (entry already exists);
 * re-planning clears `externalInit` and the retry goes through.
 */
export async function buildReleaseExternalTx(p: ReleaseExternalParams): Promise<Transaction> {
  const vaultPkg = requireId(TRADING_VAULT_PACKAGE_ID, "trading-vault package");
  const accumulatorUpdate =
    p.plan.attestTypes.length > 0
      ? await fetchHermesAccumulatorUpdate([
          ...new Set(Object.values(p.plan.feedIdByType)),
        ])
      : null;

  const tx = new Transaction();
  if (p.plan.externalInit) {
    tx.moveCall({
      target: `${p.plan.externalInit.oraclePkg}::equity_oracle::init_entry`,
      arguments: [
        tx.object(p.plan.vaultId),
        tx.object(p.plan.externalInit.bookId),
        tx.object(CLOCK_ID),
      ],
    });
  }
  const appraisal = composeAppraisal(tx, p.plan, {
    protocolConfigId: p.protocolConfigId,
    accumulatorUpdate,
  });
  tx.moveCall({
    target: `${vaultPkg}::vault::release_external`,
    typeArguments: [p.plan.depositType],
    arguments: [
      tx.object(p.plan.vaultId),
      tx.object(p.curatorCapId),
      appraisal,
      tx.pure.u64(p.amountRaw),
      tx.object(CLOCK_ID),
    ],
  });
  return tx;
}
