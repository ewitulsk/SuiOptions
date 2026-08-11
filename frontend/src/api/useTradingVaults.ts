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
  fetchTradingVaultStake,
  fetchTradingVaultTrades,
  fetchTradingVaults,
  type TradingVault,
  type TradingVaultDetail,
  type TradingVaultPpsPoint,
  type TradingVaultStake,
  type TradingVaultTrade,
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

/** Share-price history for the detail chart (SO-293). */
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

/** The connected wallet's stake in one vault (SO-293). */
export function useTradingVaultStake(vaultId: string | null, address: string | null) {
  return useQuery<TradingVaultStake, Error>({
    queryKey: ["trading-vault-stake", vaultId, address],
    enabled: vaultId !== null && address !== null,
    queryFn: () => fetchTradingVaultStake(vaultId as string, address as string),
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
 * asset with no Pyth feed). Re-plans when the vault's holdings move, or when
 * the chosen deposit asset changes (SO-370 — a non-accounting deposit adds
 * its own attest leg).
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
      depositAsset ?? null,
    ],
    enabled: vault !== null,
    staleTime: 60_000,
    retry: 1,
    queryFn: () => planAppraisal(client, vault as TradingVaultDetail, depositAsset),
  });
}

/** One pending withdrawal-queue entry, read from the vault object (SO-370). */
export type PendingWithdrawRequest = {
  /** Queue sequence number — `amend_payout_asset`'s handle. */
  seq: bigint;
  recipient: string;
  /** u128 decimal string, atomic share units (virtual-offset scale). */
  shares: string;
  /** Canonical coin type the recipient asked to be paid in. */
  payoutAsset: string;
  requestedAtMs: number | null;
};

/** The vault object's SO-370 multi-asset config + pending withdrawal queue,
 * read straight from chain (no service serves either yet). */
export type TradingVaultOnchain = {
  /** Canonical deposit/payout allowlist (`config.deposit_assets`); always
   * contains the accounting asset. */
  depositAssets: string[];
  entryHaircutBps: number;
  exitHaircutBps: number;
  requests: PendingWithdrawRequest[];
};

/** Cap on queue entries read per refresh — `pendingWithdrawals` counts the
 * rest; the queue is FIFO so the head entries are the actionable ones. */
const MAX_QUEUE_READ = 50;

function u64Field(v: unknown): number | null {
  return typeof v === "string" || typeof v === "number" ? Number(v) : null;
}

/**
 * Read the vault object's deposit-asset allowlist, haircuts, and pending
 * withdrawal requests (SO-370). Queue entries live in a `Table<u64,
 * WithdrawRequest>` walked `queue_head..queue_tail` via client-side field-id
 * derivation — the same posture as the other Table reads in `tx/appraisal.ts`.
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
      const head = u64Field(fields?.queue_head) ?? 0;
      const tail = u64Field(fields?.queue_tail) ?? 0;
      const queueTableId = idString(fields?.queue);

      const requests: PendingWithdrawRequest[] = [];
      if (queueTableId && tail > head) {
        const seqs: number[] = [];
        for (let s = head; s < tail && seqs.length < MAX_QUEUE_READ; s++) seqs.push(s);
        const fieldIds = seqs.map((s) =>
          deriveDynamicFieldID(queueTableId, "u64", bcs.u64().serialize(s).toBytes()),
        );
        const { objects } = await client.core.getObjects({
          objectIds: fieldIds,
          include: { json: true },
        });
        for (let i = 0; i < seqs.length; i++) {
          const entry = objects[i];
          if (entry instanceof Error) continue; // fulfilled between reads
          const req = structFields(structFields(entry.json)?.value);
          const recipient = typeof req?.recipient === "string" ? req.recipient : null;
          const shares = req?.shares;
          const payout = typeNameString(req?.payout_asset);
          if (!recipient || !payout || (typeof shares !== "string" && typeof shares !== "number")) {
            continue;
          }
          requests.push({
            seq: BigInt(seqs[i]),
            recipient,
            shares: String(shares),
            payoutAsset: canon(payout),
            requestedAtMs: u64Field(req?.requested_at_ms),
          });
        }
      }
      return {
        depositAssets,
        entryHaircutBps: u64Field(cfg?.entry_haircut_bps) ?? 0,
        exitHaircutBps: u64Field(cfg?.exit_haircut_bps) ?? 0,
        requests,
      };
    },
  });
}
