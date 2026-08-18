// Exchange maker-escrow PTB builders (SO-416).
//
// Maker (limit) orders settle out of a shared `exchange::balance_manager::
// BalanceManager` the maker owns — takers never need one. "Enable escrow"
// creates it once (`balance_manager::new`); each resting order's
// `makerAmount` must be covered by a `deposit` (ingress-gated on the shared
// Whitelist, SO-384); `withdraw` is owner-only, instant, and never gated.

import { Transaction, coinWithBalance } from "@mysten/sui/transactions";
import type { SuiGrpcClient } from "@mysten/sui/grpc";

import { ENV, EXCHANGE_PACKAGE_ID, WHITELIST_ID } from "../config";

function requireExchange(): { pkg: string; whitelist: string } {
  if (!EXCHANGE_PACKAGE_ID || !WHITELIST_ID) {
    throw new Error(
      `No exchange deployment for VITE_ENVIRONMENT="${ENV}" (token-info returned no exchange/whitelist ids)`,
    );
  }
  return { pkg: EXCHANGE_PACKAGE_ID, whitelist: WHITELIST_ID };
}

/**
 * One-time "enable escrow" PTB: `balance_manager::new()` shares a manager
 * owned by the sender and returns its ID (dropped in the PTB — resolve the
 * created object from the tx effects via
 * {@link resolveCreatedBalanceManager}).
 */
export function buildEnableEscrowTx(): Transaction {
  const { pkg } = requireExchange();
  const tx = new Transaction();
  tx.moveCall({ target: `${pkg}::balance_manager::new` });
  return tx;
}

/**
 * The shared `BalanceManager` created by an enable-escrow transaction.
 * Same created-object resolution pattern as tx/appraisal.ts; retries briefly
 * because the fullnode may not serve the tx immediately.
 */
export async function resolveCreatedBalanceManager(
  client: SuiGrpcClient,
  digest: string,
): Promise<string> {
  let lastErr: unknown = null;
  for (let attempt = 0; attempt < 5; attempt++) {
    try {
      const res = await client.core.getTransaction({
        digest,
        include: { effects: true, objectTypes: true },
      });
      const txn = res.Transaction ?? res.FailedTransaction;
      if (txn.status && !txn.status.success) {
        throw new Error(`enable escrow failed on-chain: ${JSON.stringify(txn.status.error)}`);
      }
      const types = txn.objectTypes ?? {};
      for (const change of txn.effects?.changedObjects ?? []) {
        if (change.idOperation !== "Created") continue;
        if (types[change.objectId]?.endsWith("::balance_manager::BalanceManager")) {
          return change.objectId;
        }
      }
      throw new Error("no BalanceManager in the transaction's created objects");
    } catch (err) {
      lastErr = err;
      // "failed on-chain" is final; not-yet-indexed reads are worth retrying.
      if (err instanceof Error && err.message.startsWith("enable escrow failed")) throw err;
      await new Promise((r) => setTimeout(r, 800));
    }
  }
  throw new Error(`could not resolve the new BalanceManager: ${lastErr}`);
}

/** Deposit `amount` of `coinType` from the wallet into the maker's escrow. */
export function buildEscrowDepositTx(p: {
  bmId: string;
  coinType: string;
  amount: bigint;
}): Transaction {
  const { pkg, whitelist } = requireExchange();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::balance_manager::deposit`,
    typeArguments: [p.coinType],
    arguments: [
      tx.object(p.bmId),
      tx.object(whitelist),
      coinWithBalance({ type: p.coinType, balance: p.amount }),
    ],
  });
  return tx;
}

/** Withdraw `amount` of `coinType` from the escrow back to `recipient`.
 * Owner-only on-chain; independent of any pause state. */
export function buildEscrowWithdrawTx(p: {
  bmId: string;
  coinType: string;
  amount: bigint;
  recipient: string;
}): Transaction {
  const { pkg } = requireExchange();
  const tx = new Transaction();
  const coin = tx.moveCall({
    target: `${pkg}::balance_manager::withdraw`,
    typeArguments: [p.coinType],
    arguments: [tx.object(p.bmId), tx.pure.u64(p.amount)],
  });
  tx.transferObjects([coin], p.recipient);
  return tx;
}
