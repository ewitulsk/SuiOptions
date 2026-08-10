// registry::create_market<Base, Quote> PTB (contracts/exchange/sources/
// registry.move). The returned ID is dropped in the PTB — the shared
// SettlementRegistry is resolved from the transaction's created objects.

import { Transaction } from "@mysten/sui/transactions";
import type { SuiGrpcClient } from "@mysten/sui/grpc";

export function buildCreateMarketTx(params: {
  packageId: string;
  adminCapId: string;
  base: string;
  quote: string;
  tickSize: bigint;
  minSize: bigint;
  feeBps: bigint;
}): Transaction {
  const tx = new Transaction();
  tx.moveCall({
    target: `${params.packageId}::registry::create_market`,
    typeArguments: [params.base, params.quote],
    arguments: [
      tx.object(params.adminCapId),
      tx.pure.u64(params.tickSize),
      tx.pure.u64(params.minSize),
      tx.pure.u64(params.feeBps),
    ],
  });
  return tx;
}

/**
 * The shared `SettlementRegistry` created by a create_market transaction.
 * Same created-object resolution pattern as frontend/src/tx/appraisal.ts;
 * retries briefly because the fullnode may not serve the tx immediately.
 */
export async function resolveRegistryId(client: SuiGrpcClient, digest: string): Promise<string> {
  let lastErr: unknown = null;
  for (let attempt = 0; attempt < 5; attempt++) {
    try {
      const res = await client.core.getTransaction({
        digest,
        include: { effects: true, objectTypes: true },
      });
      const txn = res.Transaction ?? res.FailedTransaction;
      if (txn.status && !txn.status.success) {
        throw new Error(`create_market failed on-chain: ${JSON.stringify(txn.status.error)}`);
      }
      const types = txn.objectTypes ?? {};
      for (const change of txn.effects?.changedObjects ?? []) {
        if (change.idOperation !== "Created") continue;
        if (types[change.objectId]?.includes("::registry::SettlementRegistry<")) {
          return change.objectId;
        }
      }
      throw new Error("no SettlementRegistry in the transaction's created objects");
    } catch (err) {
      lastErr = err;
      // "failed on-chain" is final; not-yet-indexed reads are worth retrying.
      if (err instanceof Error && err.message.startsWith("create_market failed")) throw err;
      await new Promise((r) => setTimeout(r, 800));
    }
  }
  throw new Error(`could not resolve the new SettlementRegistry: ${lastErr}`);
}
