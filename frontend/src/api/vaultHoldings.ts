// Trading-vault position discovery (SO-303), shared between the SO-289
// appraisal composer (`tx/appraisal.ts`) and the vault-detail positions UI.
//
// `classifyVaultPositions` reads the vault's custodied position objects over
// gRPC and classifies them by Move type: DeepBook custodies (held assets +
// active pools), RFQ tickets, written option positions (bucket type args),
// and held option coins. Strict mode (the composer) throws on anything
// unreadable or unrecognized; tolerant mode (the UI) downgrades that
// position to `unclassified` so one odd object never blanks the card.

import { useQuery } from "@tanstack/react-query";
import type { SuiGrpcClient } from "@mysten/sui/grpc";
import { normalizeStructTag, parseStructTag } from "@mysten/sui/utils";

import { useSuiGrpcClient } from "../lib/suiGrpc";
import { fetchBuckets, optionCoinType, seriesOptionType } from "./client";
import type { TradingVaultDetail } from "./tradingVaults";

// ══════════════════════════ Move-JSON tolerant reads ══════════════════════════

// The gRPC `json` rendering of Move values differs from JSON-RPC's (structs
// may or may not nest under `fields`; TypeName/ID may render as bare
// strings). These helpers accept both shapes.

export function asRecord(v: unknown): Record<string, unknown> | null {
  return typeof v === "object" && v !== null && !Array.isArray(v)
    ? (v as Record<string, unknown>)
    : null;
}

export function structFields(v: unknown): Record<string, unknown> | null {
  const r = asRecord(v);
  if (!r) return null;
  return asRecord(r.fields) ?? r;
}

export function vecSetItems(v: unknown): unknown[] {
  if (Array.isArray(v)) return v;
  const c = structFields(v)?.contents;
  return Array.isArray(c) ? c : [];
}

export function typeNameString(v: unknown): string | null {
  if (typeof v === "string") return v;
  const n = structFields(v)?.name;
  return typeof n === "string" ? n : null;
}

export function idString(v: unknown): string | null {
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

/** Canonicalize a Move type string (chain `TypeName`s lack the `0x`). */
export function canon(t: string): string {
  return normalizeStructTag(t);
}

export function shortType(t: string): string {
  const parts = t.split("::");
  return parts.length === 3 ? `${parts[0].slice(0, 8)}…::${parts[2]}` : t;
}

// ══════════════════════════════ classification ══════════════════════════════

export type PoolLegPlan = {
  poolId: string;
  /** Canonical coin types from the pool's `Pool<B, Q>` type args. */
  baseType: string;
  quoteType: string;
};

export type CustodyPlan = {
  custodyId: string;
  /** Every tracked manager asset (canonical), deposit asset included. */
  assets: string[];
  pools: PoolLegPlan[];
};

/** Exchange-adapter custody (SO-370): authority over a SHARED exchange
 * `BalanceManager`, valued from the manager's live balances. */
export type ExchangeCustodyPlan = {
  custodyId: string;
  /** Shared exchange `BalanceManager` the custody's OwnerCap controls. */
  bmId: string;
  /** Tracked manager assets (canonical); empty on a direct custody. */
  assets: string[];
  /** SO-372 direct-escrow mode: identity-only manager — fund/defund refuse. */
  direct: boolean;
};

export type RfqTicketPlan = {
  ticketId: string;
  /** Canonical escrow coin type — the `E` type arg. */
  escrowType: string;
};

export type OptionPositionPlan = {
  positionId: string;
  bucketId: string;
  isPut: boolean;
  /** Canonical `Bucket<U,S,C>` / `PutBucket<U,S,P>` type args, in order. */
  bucketTypeArgs: [string, string, string];
  /** VaultMm-tagged positions must appraise through `vault_mm` — the
   * appraisal witness has to match the position's adapter tag. */
  viaVaultMm: boolean;
};

/** A held option coin custodied as a position (vault_mm writer flow). */
export type CoinPositionPlan = {
  positionId: string;
  /** Canonical option coin type — the `Coin<T>` type arg. */
  coinType: string;
};

export type ClassifiedPositions = {
  custodies: CustodyPlan[];
  exchangeCustodies: ExchangeCustodyPlan[];
  rfqTickets: RfqTicketPlan[];
  optionPositions: OptionPositionPlan[];
  coinPositions: CoinPositionPlan[];
  /** Tolerant mode only: position ids that couldn't be read or classified. */
  unclassified: string[];
};

/**
 * Read + classify custodied position objects (the caller passes the ACTIVE
 * rows from the api-service detail endpoint — closed positions' objects are
 * gone). Strict by default: any unreadable or unrecognized object throws
 * with a human-readable reason, which the appraisal composer surfaces as
 * the disabled-deposit reason. `{ tolerant: true }` collects those ids in
 * `unclassified` instead.
 */
export async function classifyVaultPositions(
  client: SuiGrpcClient,
  positions: { positionId: string; adapter: string }[],
  opts?: { tolerant?: boolean },
): Promise<ClassifiedPositions> {
  const tolerant = opts?.tolerant ?? false;
  const custodies: CustodyPlan[] = [];
  const exchangeCustodies: ExchangeCustodyPlan[] = [];
  const rfqTickets: RfqTicketPlan[] = [];
  const optionPositions: OptionPositionPlan[] = [];
  const coinPositions: CoinPositionPlan[] = [];
  const unclassified: string[] = [];
  if (positions.length === 0) {
    return { custodies, exchangeCustodies, rfqTickets, optionPositions, coinPositions, unclassified };
  }

  const { objects } = await client.core.getObjects({
    objectIds: positions.map((p) => p.positionId),
    include: { json: true },
  });
  const bucketNeeded: { positionId: string; bucketId: string; viaVaultMm: boolean }[] = [];
  for (let i = 0; i < positions.length; i++) {
    const obj = objects[i];
    try {
      if (obj instanceof Error) {
        throw new Error(`Couldn't read position ${positions[i].positionId}: ${obj.message}`);
      }
      const type = obj.type;
      const fields = structFields(obj.json) ?? {};
      if (type.endsWith("::deepbook_adapter::DeepBookCustody")) {
        const assets = vecSetItems(fields.assets)
          .map(typeNameString)
          .filter((t): t is string => t !== null)
          .map(canon);
        const poolIds = vecSetItems(fields.active_pools)
          .map(idString)
          .filter((id): id is string => id !== null);
        const pools: PoolLegPlan[] = [];
        if (poolIds.length > 0) {
          const poolObjs = await client.core.getObjects({ objectIds: poolIds, include: {} });
          for (let j = 0; j < poolIds.length; j++) {
            const pool = poolObjs.objects[j];
            if (pool instanceof Error) {
              throw new Error(`Couldn't read DeepBook pool ${poolIds[j]}: ${pool.message}`);
            }
            const params = parseStructTag(pool.type).typeParams;
            if (params.length < 2) throw new Error(`Unexpected pool type: ${pool.type}`);
            pools.push({
              poolId: poolIds[j],
              baseType: normalizeStructTag(params[0]),
              quoteType: normalizeStructTag(params[1]),
            });
          }
        }
        custodies.push({ custodyId: obj.objectId, assets, pools });
      } else if (type.endsWith("::exchange_adapter::ExchangeCustody")) {
        const bmId = idString(fields.bm_id);
        if (!bmId) throw new Error(`Exchange custody ${obj.objectId} has no bm_id`);
        const assets = vecSetItems(fields.assets)
          .map(typeNameString)
          .filter((t): t is string => t !== null)
          .map(canon);
        exchangeCustodies.push({
          custodyId: obj.objectId,
          bmId,
          assets,
          direct: fields.direct === true,
        });
      } else if (type.endsWith("::options_adapter::RfqTicket")) {
        const escrow = typeNameString(fields.escrow_type);
        if (!escrow) throw new Error(`RFQ ticket ${obj.objectId} has no escrow_type`);
        rfqTickets.push({ ticketId: obj.objectId, escrowType: canon(escrow) });
      } else if (type.endsWith("::position::Position") || "range_start" in fields) {
        const bucketId = idString(fields.bucket_id);
        if (!bucketId) throw new Error(`Option position ${obj.objectId} has no bucket_id`);
        bucketNeeded.push({
          positionId: obj.objectId,
          bucketId,
          viaVaultMm: positions[i].adapter.endsWith("::vault_mm::VaultMm"),
        });
      } else if (type.includes("::coin::Coin<")) {
        const inner = parseStructTag(type).typeParams[0];
        coinPositions.push({ positionId: obj.objectId, coinType: normalizeStructTag(inner) });
      } else {
        throw new Error(`Unrecognized custodied position type ${shortType(type)}`);
      }
    } catch (err) {
      if (!tolerant) throw err;
      unclassified.push(positions[i].positionId);
    }
  }
  if (bucketNeeded.length > 0) {
    const bucketObjs = await client.core.getObjects({
      objectIds: bucketNeeded.map((b) => b.bucketId),
      include: {},
    });
    for (let i = 0; i < bucketNeeded.length; i++) {
      try {
        const bucket = bucketObjs.objects[i];
        if (bucket instanceof Error) {
          throw new Error(`Couldn't read bucket ${bucketNeeded[i].bucketId}: ${bucket.message}`);
        }
        const tag = parseStructTag(bucket.type);
        const isPut = tag.name === "PutBucket";
        if (!isPut && tag.name !== "Bucket") {
          throw new Error(`Unexpected bucket type: ${bucket.type}`);
        }
        if (tag.typeParams.length < 3) throw new Error(`Unexpected bucket type: ${bucket.type}`);
        optionPositions.push({
          positionId: bucketNeeded[i].positionId,
          bucketId: bucketNeeded[i].bucketId,
          viaVaultMm: bucketNeeded[i].viaVaultMm,
          isPut,
          bucketTypeArgs: [
            normalizeStructTag(tag.typeParams[0]),
            normalizeStructTag(tag.typeParams[1]),
            normalizeStructTag(tag.typeParams[2]),
          ],
        });
      } catch (err) {
        if (!tolerant) throw err;
        unclassified.push(bucketNeeded[i].positionId);
      }
    }
  }
  return { custodies, exchangeCustodies, rfqTickets, optionPositions, coinPositions, unclassified };
}

// ═══════════════════════════ UI holdings hook ═══════════════════════════

/** Bucket identity from the api-service catalog, for display. */
export type OptionBucketInfo = {
  assetSymbol: string;
  /** Display-scaled strike; null if decimals unknown. */
  strike: number | null;
  expiryMs: number;
  isPut: boolean;
};

export type VaultHolding =
  | { kind: "custody"; assets: string[]; pools: PoolLegPlan[] }
  | { kind: "exchangeCustody"; bmId: string; assets: string[]; direct: boolean }
  | { kind: "rfq"; escrowType: string }
  | {
      kind: "option";
      isPut: boolean;
      /** Canonical underlying coin type (bucket type arg 0). */
      underlying: string;
      viaVaultMm: boolean;
      /** Catalog detail (strike/expiry); null when the bucket isn't served. */
      bucket: OptionBucketInfo | null;
    }
  | { kind: "optionCoin"; coinType: string; bucket: OptionBucketInfo | null };

/**
 * Classified detail for a vault's ACTIVE custodied positions, keyed by
 * position id (SO-303). Tolerant: unreadable/unrecognized positions are
 * simply absent from the map, and a missing bucket-catalog entry only
 * drops the strike/expiry detail — the card falls back to adapter names.
 */
export function useVaultHoldings(vault: TradingVaultDetail | null) {
  const client = useSuiGrpcClient();
  const active = vault?.positions.filter((p) => p.active) ?? [];
  return useQuery<Map<string, VaultHolding>, Error>({
    queryKey: [
      "trading-vault-holdings",
      vault?.vaultId ?? null,
      vault?.positionCount ?? 0,
      vault?.updatedAtMs ?? 0,
    ],
    enabled: vault !== null && active.length > 0,
    staleTime: 60_000,
    retry: 1,
    queryFn: async () => {
      const c = await classifyVaultPositions(client, active, { tolerant: true });

      // Bucket identity (strike/expiry/underlying symbol) from the same
      // api-service catalog the appraisal composer prices option coins with.
      const byBucketId = new Map<string, OptionBucketInfo>();
      const byCoinType = new Map<string, OptionBucketInfo>();
      if (c.optionPositions.length > 0 || c.coinPositions.length > 0) {
        try {
          for (const s of await fetchBuckets()) {
            for (const b of s.buckets) {
              const info: OptionBucketInfo = {
                assetSymbol: s.asset_symbol,
                strike: b.strike,
                expiryMs: s.expiry_ms,
                isPut: seriesOptionType(s) === "put",
              };
              if (b.bucket_id) byBucketId.set(b.bucket_id, info);
              byCoinType.set(canon(optionCoinType(b)), info);
            }
          }
        } catch {
          // Catalog down — rows still render from the bucket type args.
        }
      }

      const map = new Map<string, VaultHolding>();
      for (const cu of c.custodies) {
        map.set(cu.custodyId, { kind: "custody", assets: cu.assets, pools: cu.pools });
      }
      for (const xc of c.exchangeCustodies) {
        map.set(xc.custodyId, {
          kind: "exchangeCustody",
          bmId: xc.bmId,
          assets: xc.assets,
          direct: xc.direct,
        });
      }
      for (const t of c.rfqTickets) {
        map.set(t.ticketId, { kind: "rfq", escrowType: t.escrowType });
      }
      for (const p of c.optionPositions) {
        map.set(p.positionId, {
          kind: "option",
          isPut: p.isPut,
          underlying: p.bucketTypeArgs[0],
          viaVaultMm: p.viaVaultMm,
          bucket: byBucketId.get(p.bucketId) ?? null,
        });
      }
      for (const cp of c.coinPositions) {
        map.set(cp.positionId, {
          kind: "optionCoin",
          coinType: cp.coinType,
          bucket: byCoinType.get(cp.coinType) ?? null,
        });
      }
      return map;
    },
  });
}
