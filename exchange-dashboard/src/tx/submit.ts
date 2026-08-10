// Wallet-paid transaction submit — the sponsorship-free trim of
// frontend/src/tx/submit.ts. Build (resolution + gas selection) via gRPC,
// wallet signs the exact bytes, execute via gRPC.

import { useCurrentAccount, useSignTransaction } from "@mysten/dapp-kit";
import type { Transaction } from "@mysten/sui/transactions";
import { fromBase64, toBase64 } from "@mysten/sui/utils";

import { useSuiGrpcClient } from "../lib/suiGrpc";

export function useSubmitTransaction() {
  const account = useCurrentAccount();
  const client = useSuiGrpcClient();
  const { mutateAsync: signTransaction } = useSignTransaction();

  /** Builds, signs, executes; returns the digest. */
  return async function submit(tx: Transaction): Promise<string> {
    if (!account) throw new Error("connect a wallet first");
    tx.setSenderIfNotSet(account.address);
    const built = toBase64(await tx.build({ client }));
    const { bytes: signedBytes, signature } = await signTransaction({ transaction: built });
    const res = await client.core.executeTransaction({
      transaction: fromBase64(signedBytes),
      signatures: [signature],
    });
    // A FailedTransaction still has a digest — callers inspect status/effects.
    return (res.Transaction ?? res.FailedTransaction).digest;
  };
}
