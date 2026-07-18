import { useQuery } from "@tanstack/react-query";

import { TRADING_VAULT_PUBLISH_DIGEST } from "../config";
import { useSuiGrpcClient } from "../lib/suiGrpc";
import {
  fetchTradingVault,
  fetchTradingVaults,
  type TradingVault,
  type TradingVaultDetail,
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

/**
 * The shared `VaultProtocolConfig` object id for the trading_vault deployment.
 * token-info doesn't serve it, so it's resolved client-side ONCE from the
 * package's publish transaction (the object created with type
 * `…::registry::VaultProtocolConfig`) and cached for the session.
 *
 * `null` when the object can't be found in the publish tx; the query is
 * disabled entirely when the network has no trading-vault deployment.
 */
export function useVaultProtocolConfigId() {
  const client = useSuiGrpcClient();
  return useQuery<string | null, Error>({
    queryKey: ["trading-vault-protocol-config", TRADING_VAULT_PUBLISH_DIGEST],
    enabled: !!TRADING_VAULT_PUBLISH_DIGEST,
    staleTime: Infinity,
    queryFn: async () => {
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
