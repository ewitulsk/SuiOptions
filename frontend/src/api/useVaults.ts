import { useQuery } from "@tanstack/react-query";
import { useSuiClient } from "@mysten/dapp-kit";
import { normalizeStructTag } from "@mysten/sui/utils";

import { PACKAGE_ID } from "../config";
import {
  fetchRfqBids,
  fetchVault,
  fetchVaultApy,
  fetchVaultRfqs,
  fetchVaultRounds,
  fetchVaults,
  type RfqBid,
  type Vault,
  type VaultApySeries,
  type VaultRfq,
  type VaultRound,
} from "./vaults";

export function useVaults() {
  return useQuery<Vault[], Error>({
    queryKey: ["vaults"],
    queryFn: fetchVaults,
    // Vault state (round, pps, tvl) only moves on a crank — a slower poll than
    // the order book is plenty.
    refetchInterval: 15_000,
  });
}

export function useVault(vaultId: string | null) {
  return useQuery<Vault, Error>({
    queryKey: ["vault", vaultId],
    enabled: vaultId !== null,
    queryFn: () => fetchVault(vaultId as string),
    refetchInterval: 15_000,
  });
}

export function useVaultRounds(vaultId: string | null) {
  return useQuery<VaultRound[], Error>({
    queryKey: ["vault-rounds", vaultId],
    enabled: vaultId !== null,
    queryFn: () => fetchVaultRounds(vaultId as string),
    refetchInterval: 30_000,
  });
}

/**
 * The vault's APY-over-time curve: realized (indexer) + predicted
 * (price-charting apy sampler). Predicted is empty when that isn't deployed,
 * so the chart shows realized-only until then.
 */
export function useVaultApyHistory(vaultId: string | null) {
  return useQuery<VaultApySeries, Error>({
    queryKey: ["vault-apy-history", vaultId],
    enabled: vaultId !== null,
    queryFn: () => fetchVaultApy(vaultId as string),
    refetchInterval: 60_000,
  });
}

/** Every RFQ a vault originated — backs the per-round premium breakdown. */
export function useVaultRfqs(vaultId: string | null) {
  return useQuery<VaultRfq[], Error>({
    queryKey: ["vault-rfqs", vaultId],
    enabled: vaultId !== null,
    queryFn: () => fetchVaultRfqs(vaultId as string),
    refetchInterval: 30_000,
  });
}

/** Bid history for one auction; lazy — only fetched when `enabled`. */
export function useRfqBids(rfqId: string | null, enabled: boolean) {
  return useQuery<RfqBid[], Error>({
    queryKey: ["rfq-bids", rfqId],
    enabled: enabled && rfqId !== null,
    queryFn: () => fetchRfqBids(rfqId as string),
  });
}

/** A custodied/owned receipt object, with the id needed to build its PTB. */
export type OwnedReceipt = {
  object_id: string;
  vault_id: string;
  round: number;
  /** Deposits: queued underlying. Withdrawals: escrowed shares. Raw units. */
  amount_raw: string;
};

export type OwnedVaultReceipts = {
  deposits: OwnedReceipt[];
  withdraws: OwnedReceipt[];
};

type ReceiptFields = {
  vault_id: string;
  round: string;
  amount?: string;
  shares?: string;
};

function parseReceipt(
  data: { objectId: string; content?: unknown } | null | undefined,
  wantShares: boolean,
): OwnedReceipt | null {
  const content = data?.content as { dataType?: string; fields?: ReceiptFields } | undefined;
  if (!data || !content || content.dataType !== "moveObject" || !content.fields) return null;
  const f = content.fields;
  const amount = wantShares ? f.shares : f.amount;
  if (amount == null) return null;
  return {
    object_id: data.objectId,
    vault_id: f.vault_id,
    round: Number(f.round),
    amount_raw: String(amount),
  };
}

/**
 * The connected user's `DepositReceipt` / `WithdrawReceipt` objects for a
 * vault, with object ids — the inputs `claim_shares` / `complete_withdraw` /
 * `instant_withdraw_pending` PTBs need.
 *
 * The api-service `/receipts` endpoint aggregates by round and carries no
 * object id, so the actions read from the chain instead (mirrors
 * `useOwnedPositions`):
 *   - **wallet** → `getOwnedObjects` filtered to the receipt struct types.
 *   - **session** → the receipts are custodied on the options Account; pass
 *     `custodyObjectIds` (the on-account `ObjectIndexKey` list) and we read
 *     those by id.
 */
export function useOwnedVaultReceipts(
  wallet: string | null,
  vaultId: string | null,
  custodyObjectIds?: string[] | null,
) {
  const client = useSuiClient();
  const custodyKey = custodyObjectIds ? custodyObjectIds.slice().sort().join(",") : null;
  return useQuery<OwnedVaultReceipts, Error>({
    queryKey: ["vault-receipts-owned", wallet, vaultId, PACKAGE_ID, custodyKey],
    enabled: wallet !== null && vaultId !== null && !!PACKAGE_ID,
    refetchInterval: 10_000,
    queryFn: async () => {
      const empty: OwnedVaultReceipts = { deposits: [], withdraws: [] };
      if (!wallet || !vaultId || !PACKAGE_ID) return empty;
      const depType = `${PACKAGE_ID}::vault::DepositReceipt`;
      const wdType = `${PACKAGE_ID}::vault::WithdrawReceipt`;

      // Session custody: read the indexed object ids directly.
      if (custodyObjectIds) {
        if (custodyObjectIds.length === 0) return empty;
        const objs = await client.multiGetObjects({
          ids: custodyObjectIds,
          options: { showContent: true, showType: true },
        });
        const out: OwnedVaultReceipts = { deposits: [], withdraws: [] };
        for (const item of objs) {
          const type = item.data?.type ?? "";
          if (type.startsWith(depType)) {
            const r = parseReceipt(item.data, false);
            if (r && r.vault_id === vaultId) out.deposits.push(r);
          } else if (type.startsWith(wdType)) {
            const r = parseReceipt(item.data, true);
            if (r && r.vault_id === vaultId) out.withdraws.push(r);
          }
        }
        return out;
      }

      // Wallet: query owned objects by struct type.
      const collect = async (structType: string, wantShares: boolean) => {
        const out: OwnedReceipt[] = [];
        let cursor: string | null | undefined = undefined;
        do {
          const page = await client.getOwnedObjects({
            owner: wallet,
            filter: { StructType: structType },
            options: { showContent: true },
            cursor,
          });
          for (const item of page.data) {
            const r = parseReceipt(item.data, wantShares);
            if (r && r.vault_id === vaultId) out.push(r);
          }
          cursor = page.hasNextPage ? page.nextCursor : undefined;
        } while (cursor);
        return out;
      };

      const [deposits, withdraws] = await Promise.all([
        collect(depType, false),
        collect(wdType, true),
      ]);
      return { deposits, withdraws };
    },
  });
}

/**
 * The user's spendable share-coin (`Coin<V>`) balance for a vault, raw units.
 * Wallet users hold it at their address; session users hold it in custody
 * (pass `custodyBalances`, the on-account balance map keyed by canonical type).
 */
export function useShareBalance(
  wallet: string | null,
  shareType: string | null,
  custodyBalances?: Record<string, bigint> | null,
) {
  const client = useSuiClient();
  return useQuery<bigint, Error>({
    queryKey: ["vault-share-balance", wallet, shareType, custodyBalances ? "session" : "wallet"],
    enabled: wallet !== null && shareType !== null,
    refetchInterval: 10_000,
    queryFn: async () => {
      if (!wallet || !shareType) return 0n;
      const canonical = normalizeStructTag(shareType);
      if (custodyBalances) {
        return custodyBalances[canonical] ?? 0n;
      }
      const bal = await client.getBalance({ owner: wallet, coinType: shareType });
      return BigInt(bal.totalBalance);
    },
  });
}
