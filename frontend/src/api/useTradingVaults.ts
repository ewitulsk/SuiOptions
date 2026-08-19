import { useQuery } from "@tanstack/react-query";
import { bcs } from "@mysten/sui/bcs";
import { deriveDynamicFieldID, normalizeStructTag, parseStructTag } from "@mysten/sui/utils";

import { TRADING_VAULT_OBJECTS, TRADING_VAULT_PUBLISH_DIGEST } from "../config";
import { useSuiGrpcClient } from "../lib/suiGrpc";
import { planAppraisal, type AppraisalPlan } from "../tx/appraisal";
import { asRecord, canon, idString, structFields, typeNameString, vecSetItems } from "./vaultHoldings";
import {
  fetchTradingVault,
  fetchTradingVaultPpsHistory,
  fetchTradingVaultTrades,
  fetchTradingVaults,
  fetchVaultPendingRequests,
  fetchVaultPositionDetail,
  fetchVaultPositions,
  fetchVaultSettlement,
  fetchVaultWaterfall,
  type LaneLabel,
  type TradingVault,
  type TradingVaultDetail,
  type TradingVaultPpsPoint,
  type TradingVaultTrade,
  type VaultPendingRequest,
  type VaultPosition,
  type VaultPositionDetail,
  type VaultSettlement,
  type VaultWaterfall,
} from "./tradingVaults";

export function useTradingVaults() {
  return useQuery<TradingVault[], Error>({
    queryKey: ["trading-vaults"],
    queryFn: fetchTradingVaults,
    // Vault state (shares, pps, queue depth) moves on user/curator actions —
    // the same slow poll the covered-call vaults use is plenty.
    refetchInterval: 15_000,
  });
}

export function useTradingVault(vaultId: string | null) {
  return useQuery<TradingVaultDetail, Error>({
    queryKey: ["trading-vault", vaultId],
    enabled: vaultId !== null,
    queryFn: () => fetchTradingVault(vaultId as string),
    refetchInterval: 15_000,
  });
}

/** Per-tranche share-price history for the detail chart (SO-293/SO-418). */
export function useTradingVaultPpsHistory(vaultId: string | null) {
  return useQuery<TradingVaultPpsPoint[], Error>({
    queryKey: ["trading-vault-pps-history", vaultId],
    enabled: vaultId !== null,
    queryFn: () => fetchTradingVaultPpsHistory(vaultId as string),
    refetchInterval: 30_000,
  });
}

/** Curator spot trades for the detail page's recent-trades list (SO-313). */
export function useTradingVaultTrades(vaultId: string | null) {
  return useQuery<TradingVaultTrade[], Error>({
    queryKey: ["trading-vault-trades", vaultId],
    enabled: vaultId !== null,
    queryFn: () => fetchTradingVaultTrades(vaultId as string),
    refetchInterval: 30_000,
  });
}

/** The connected wallet's `VaultPosition` NFTs in one vault (SO-418 —
 * replaces the address-keyed stake). */
export function useVaultPositions(vaultId: string | null, address: string | null) {
  return useQuery<VaultPosition[], Error>({
    queryKey: ["trading-vault-positions", vaultId, address],
    enabled: vaultId !== null && address !== null,
    queryFn: () => fetchVaultPositions(vaultId as string, address as string),
    refetchInterval: 15_000,
  });
}

/** One position by id — works for ANY holder (secondary-buyer due
 * diligence, SO-418). */
export function usePositionDetail(positionId: string | null) {
  return useQuery<VaultPositionDetail, Error>({
    queryKey: ["trading-vault-position-detail", positionId],
    enabled: positionId !== null,
    queryFn: () => fetchVaultPositionDetail(positionId as string),
    refetchInterval: 15_000,
  });
}

/** The §3.4a waterfall decomposition at the latest capital sync (SO-418). */
export function useWaterfall(vaultId: string | null) {
  return useQuery<VaultWaterfall, Error>({
    queryKey: ["trading-vault-waterfall", vaultId],
    enabled: vaultId !== null,
    queryFn: () => fetchVaultWaterfall(vaultId as string),
    refetchInterval: 15_000,
    retry: 1,
  });
}

/** Terminal settlement pool state (SO-418). Only meaningful once closed. */
export function useSettlement(vaultId: string | null, enabled = true) {
  return useQuery<VaultSettlement, Error>({
    queryKey: ["trading-vault-settlement", vaultId],
    enabled: enabled && vaultId !== null,
    queryFn: () => fetchVaultSettlement(vaultId as string),
    refetchInterval: 30_000,
  });
}

/** Lane-aware pending withdraw requests with server-computed payability
 * (SO-418). */
export function usePendingRequests(vaultId: string | null) {
  return useQuery<VaultPendingRequest[], Error>({
    queryKey: ["trading-vault-pending-requests", vaultId],
    enabled: vaultId !== null,
    queryFn: () => fetchVaultPendingRequests(vaultId as string),
    refetchInterval: 15_000,
  });
}

/**
 * The shared `VaultProtocolConfig` object id for the trading_vault
 * deployment. Served directly by token-info since SO-292
 * (`packageInfo.tradingVaultObjects`); older deployments fall back to
 * resolving it client-side ONCE from the package's publish transaction (the
 * object created with type `…::registry::VaultProtocolConfig`), cached for
 * the session.
 *
 * `null` when the object can't be found in the publish tx; the query is
 * disabled entirely when the network has no trading-vault deployment.
 */
export function useVaultProtocolConfigId() {
  const client = useSuiGrpcClient();
  const servedId = TRADING_VAULT_OBJECTS?.vaultProtocolConfigId ?? null;
  return useQuery<string | null, Error>({
    queryKey: ["trading-vault-protocol-config", servedId, TRADING_VAULT_PUBLISH_DIGEST],
    enabled: !!servedId || !!TRADING_VAULT_PUBLISH_DIGEST,
    staleTime: Infinity,
    queryFn: async () => {
      if (servedId) return servedId;
      if (!TRADING_VAULT_PUBLISH_DIGEST) return null;
      const res = await client.core.getTransaction({
        digest: TRADING_VAULT_PUBLISH_DIGEST,
        include: { effects: true, objectTypes: true },
      });
      const txn = res.Transaction ?? res.FailedTransaction;
      const types = txn.objectTypes ?? {};
      for (const change of txn.effects?.changedObjects ?? []) {
        if (change.idOperation !== "Created") continue;
        const type = types[change.objectId];
        if (type?.endsWith("::registry::VaultProtocolConfig")) return change.objectId;
      }
      return null;
    },
  });
}

/** One admin-allowlisted DeepBook pool the curator may trade (SO-299). */
export type AllowlistedPool = {
  poolId: string;
  /** Canonical coin types from the pool's `Pool<B, Q>` type args. */
  baseType: string;
  quoteType: string;
};

/**
 * The deepbook-adapter's `PoolAllowlist` (shared, admin-governed), resolved
 * to pool ids + type args by reading the allowlist object and each pool.
 * No service serves this today, so it's a direct chain read; the set only
 * moves on admin action, so a long stale time is fine.
 */
export function useAllowlistedPools(enabled: boolean) {
  const client = useSuiGrpcClient();
  const listId = TRADING_VAULT_OBJECTS?.poolAllowlistId ?? null;
  return useQuery<AllowlistedPool[], Error>({
    queryKey: ["trading-vault-pool-allowlist", listId],
    enabled: enabled && listId !== null,
    staleTime: 300_000,
    queryFn: async () => {
      const res = await client.core.getObject({
        objectId: listId as string,
        include: { json: true },
      });
      const json = res.object.json as { allowed?: unknown; fields?: { allowed?: unknown } } | null;
      const ids = vecSetItems(json?.allowed ?? json?.fields?.allowed)
        .map(idString)
        .filter((id): id is string => id !== null);
      if (ids.length === 0) return [];
      const { objects } = await client.core.getObjects({ objectIds: ids, include: {} });
      const pools: AllowlistedPool[] = [];
      for (let i = 0; i < ids.length; i++) {
        const obj = objects[i];
        // A delisted-then-deleted pool shouldn't brick the tab — skip
        // unreadable entries.
        if (obj instanceof Error) continue;
        const params = parseStructTag(obj.type).typeParams;
        if (params.length < 2) continue;
        pools.push({
          poolId: ids[i],
          baseType: normalizeStructTag(params[0]),
          quoteType: normalizeStructTag(params[1]),
        });
      }
      return pools;
    },
  });
}

/**
 * Pre-flight the SO-289 appraisal composer for a vault: discover holdings and
 * resolve every Pyth leg. `data` feeds `buildAppraisedDepositTx`; an error's
 * message is the human-readable reason deposits are blocked (e.g. a held
 * asset with no Pyth feed). Re-plans when the vault's holdings move, when
 * the chosen deposit asset changes (SO-370 — a non-accounting deposit adds
 * its own attest leg), or when the capital state / junior generation moves
 * (SO-418 — a consumed appraisal snapshots `capital_seq`).
 */
export function useAppraisalPlan(vault: TradingVaultDetail | null, depositAsset?: string) {
  const client = useSuiGrpcClient();
  return useQuery<AppraisalPlan, Error>({
    queryKey: [
      "trading-vault-appraisal-plan",
      vault?.vaultId ?? null,
      vault?.positionCount ?? 0,
      vault?.updatedAtMs ?? 0,
      // The external-equity leg composes only above zero exposure (SO-310),
      // so the first release changes the plan's shape.
      vault?.externalExposure ?? "0",
      // SO-418: capital-state transitions and generation rollovers move the
      // vault's capital_seq — a stale plan would compose a doomed appraisal.
      vault?.riskStateCode ?? 0,
      vault?.activeJuniorGeneration ?? 0,
      depositAsset ?? null,
    ],
    enabled: vault !== null,
    staleTime: 60_000,
    retry: 1,
    queryFn: () => planAppraisal(client, vault as TradingVaultDetail, depositAsset),
  });
}

/** Live state of one exchange custody's shared `BalanceManager` (SO-373):
 * delegated order signers plus the manager's balance per tracked asset. */
export type ExchangeBmState = {
  signers: string[];
  /** Canonical asset type → raw balance (decimal string); null when that
   * balance field couldn't be read (shown as unknown, not zero). */
  balances: Record<string, string | null>;
};

/**
 * Read an exchange BalanceManager's approved signers and its balances for
 * the custody's tracked assets. Balances live as `TypeName -> Balance<T>`
 * dynamic fields on the manager — field ids derive client-side (chain
 * `TypeName`s are full-width without the `0x`), the same posture as the
 * queue-table reads below.
 */
export function useExchangeBm(bmId: string | null, assets: string[]) {
  const client = useSuiGrpcClient();
  return useQuery<ExchangeBmState, Error>({
    queryKey: ["trading-vault-exchange-bm", bmId, assets.join(",")],
    enabled: bmId !== null,
    refetchInterval: 15_000,
    queryFn: async () => {
      const { object } = await client.core.getObject({
        objectId: bmId as string,
        include: { json: true },
      });
      const fields = structFields(object.json) ?? asRecord(object.json);
      const signers = vecSetItems(fields?.approved_signers).filter(
        (s): s is string => typeof s === "string",
      );
      const balances: Record<string, string | null> = {};
      if (assets.length > 0) {
        const fieldIds = assets.map((t) =>
          deriveDynamicFieldID(
            bmId as string,
            "0x1::type_name::TypeName",
            bcs.string().serialize(canon(t).replace(/^0x/, "")).toBytes(),
          ),
        );
        const { objects } = await client.core.getObjects({
          objectIds: fieldIds,
          include: { json: true },
        });
        for (let i = 0; i < assets.length; i++) {
          const entry = objects[i];
          if (entry instanceof Error) {
            balances[assets[i]] = null;
            continue;
          }
          // `Balance<T>` renders either bare or as `{ value }`.
          const v = structFields(entry.json)?.value;
          const raw =
            typeof v === "string" || typeof v === "number"
              ? String(v)
              : (() => {
                  const inner = structFields(v)?.value;
                  return typeof inner === "string" || typeof inner === "number"
                    ? String(inner)
                    : null;
                })();
          balances[assets[i]] = raw;
        }
      }
      return { signers, balances };
    },
  });
}

/** One pending withdrawal-queue entry, read from the vault object (v2:
 * lane-aware, keyed by the GLOBAL sequence). */
export type PendingWithdrawRequest = {
  /** Global sequence number — `amend_payout_asset`'s handle. */
  seq: bigint;
  lane: LaneLabel;
  recipient: string;
  /** u128 decimal string, atomic share units (virtual-offset scale). */
  shares: string;
  /** Canonical coin type the recipient asked to be paid in. */
  payoutAsset: string;
  requestedAtMs: number | null;
};

/** The vault object's SO-370 multi-asset config + pending withdrawal queue,
 * read straight from chain. */
export type TradingVaultOnchain = {
  /** Canonical deposit/payout allowlist (`config.deposit_assets`); always
   * contains the accounting asset. */
  depositAssets: string[];
  entryHaircutBps: number;
  exitHaircutBps: number;
  /** Ascending by global sequence, both lanes merged. */
  requests: PendingWithdrawRequest[];
};

/** Cap on queue entries read per lane per refresh — `pendingWithdrawals`
 * counts the rest; lanes are FIFO so the head entries are the actionable
 * ones. */
const MAX_QUEUE_READ = 25;

function u64Field(v: unknown): number | null {
  return typeof v === "string" || typeof v === "number" ? Number(v) : null;
}

/**
 * Read one v2 withdrawal lane: `lane.entries` is a `Table<u64, u64>` of
 * lane-local index → GLOBAL sequence, walked `head..tail` via client-side
 * field-id derivation, then each surviving request is read out of the
 * vault's `requests: Table<u64, WithdrawRequest>` by its global sequence.
 */
async function readLane(
  client: ReturnType<typeof useSuiGrpcClient>,
  lane: unknown,
  laneLabel: LaneLabel,
  requestsTableId: string | null,
): Promise<PendingWithdrawRequest[]> {
  const f = structFields(lane);
  const head = u64Field(f?.head) ?? 0;
  const tail = u64Field(f?.tail) ?? 0;
  const entriesTableId = idString(f?.entries);
  if (!entriesTableId || !requestsTableId || tail <= head) return [];

  const idxs: number[] = [];
  for (let i = head; i < tail && idxs.length < MAX_QUEUE_READ; i++) idxs.push(i);
  const entryFieldIds = idxs.map((i) =>
    deriveDynamicFieldID(entriesTableId, "u64", bcs.u64().serialize(i).toBytes()),
  );
  const entryObjs = await client.core.getObjects({
    objectIds: entryFieldIds,
    include: { json: true },
  });
  const seqs: number[] = [];
  for (const entry of entryObjs.objects) {
    if (entry instanceof Error) continue; // fulfilled/settled between reads
    const seq = u64Field(structFields(entry.json)?.value);
    if (seq != null) seqs.push(seq);
  }
  if (seqs.length === 0) return [];

  const reqFieldIds = seqs.map((s) =>
    deriveDynamicFieldID(requestsTableId, "u64", bcs.u64().serialize(s).toBytes()),
  );
  const reqObjs = await client.core.getObjects({
    objectIds: reqFieldIds,
    include: { json: true },
  });
  const requests: PendingWithdrawRequest[] = [];
  for (let i = 0; i < seqs.length; i++) {
    const entry = reqObjs.objects[i];
    if (entry instanceof Error) continue;
    const req = structFields(structFields(entry.json)?.value);
    const recipient = typeof req?.recipient === "string" ? req.recipient : null;
    const shares = req?.shares;
    const payout = typeNameString(req?.payout_asset);
    if (!recipient || !payout || (typeof shares !== "string" && typeof shares !== "number")) {
      continue;
    }
    requests.push({
      seq: BigInt(seqs[i]),
      lane: laneLabel,
      recipient,
      shares: String(shares),
      payoutAsset: canon(payout),
      requestedAtMs: u64Field(req?.requested_at_ms),
    });
  }
  return requests;
}

/**
 * Read the vault object's deposit-asset allowlist, haircuts, and pending
 * withdrawal requests. v2 (SO-418): the queue is two per-tranche lanes
 * (`senior_lane` / `junior_lane`) whose entry tables map lane-local indices
 * to global sequences, and requests live in the global `requests` table —
 * both walked via client-side field-id derivation, the same posture as the
 * other Table reads in `tx/appraisal.ts`.
 */
export function useTradingVaultOnchain(vaultId: string | null) {
  const client = useSuiGrpcClient();
  return useQuery<TradingVaultOnchain, Error>({
    queryKey: ["trading-vault-onchain", vaultId],
    enabled: vaultId !== null,
    refetchInterval: 15_000,
    queryFn: async () => {
      const { object } = await client.core.getObject({
        objectId: vaultId as string,
        include: { json: true },
      });
      const fields = structFields(object.json) ?? asRecord(object.json);
      const cfg = structFields(fields?.config);
      const depositAssets = vecSetItems(cfg?.deposit_assets)
        .map(typeNameString)
        .filter((t): t is string => t !== null)
        .map(canon);
      const requestsTableId = idString(fields?.requests);

      const [senior, junior] = await Promise.all([
        readLane(client, fields?.senior_lane, "senior", requestsTableId),
        readLane(client, fields?.junior_lane, "junior", requestsTableId),
      ]);
      // Cross-lane order is lowest-global-sequence-first (§3.6).
      const requests = [...senior, ...junior].sort((a, b) => (a.seq < b.seq ? -1 : 1));

      return {
        depositAssets,
        entryHaircutBps: u64Field(cfg?.entry_haircut_bps) ?? 0,
        exitHaircutBps: u64Field(cfg?.exit_haircut_bps) ?? 0,
        requests,
      };
    },
  });
}
