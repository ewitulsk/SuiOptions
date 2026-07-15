// Unified transaction submit hook.
//
// One entry point for every on-chain write. When the sponsor toggle is on and
// the user is connected, it runs Sui's sponsored-transaction flow: build the
// gasless TransactionKind, get the gas station to attach gas + sign, have the
// wallet co-sign the same bytes, then execute with both signatures. When the
// toggle is off, the transaction is built here (resolution + gas selection via
// gRPC), the wallet signs the exact bytes, and execution goes through gRPC
// `ExecuteTransaction` — building before signing keeps dapp-kit's JSON-RPC
// client out of the write path entirely (see docs/sui-json-rpc-migration.md).
// Sponsorship failures are NOT silently retried wallet-paid — they surface so
// the user can choose to turn off the Gas toggle and pay their own gas.

import { useCurrentAccount, useSignTransaction } from "@mysten/dapp-kit";
import type { Transaction } from "@mysten/sui/transactions";
import { fromBase64, toBase64 } from "@mysten/sui/utils";

import { requestSponsorship } from "../api/sponsor";
import { isSponsorEnabled } from "../state/sponsor";
import { useSuiGrpcClient } from "../lib/suiGrpc";
import { posthog } from "../lib/posthog";

export function useSubmitTransaction() {
  const account = useCurrentAccount();
  const client = useSuiGrpcClient();
  const { mutateAsync: signTransaction } = useSignTransaction();

  // Execute pre-signed bytes via gRPC and return the digest.
  async function executeSigned(bytesB64: string, signatures: string[]): Promise<string> {
    const res = await client.core.executeTransaction({
      transaction: fromBase64(bytesB64),
      signatures,
    });
    // Like JSON-RPC's executeTransactionBlock, a FailedTransaction still has a
    // digest — callers watch the digest, not the execution status.
    return (res.Transaction ?? res.FailedTransaction).digest;
  }

  return async function submit(tx: Transaction): Promise<string> {
    // Wallet-paid path: toggle off or no connected account.
    if (!isSponsorEnabled() || !account) {
      if (account) tx.setSenderIfNotSet(account.address);
      const built = toBase64(await tx.build({ client }));
      const { bytes: signedBytes, signature } = await signTransaction({ transaction: built });
      return executeSigned(signedBytes, [signature]);
    }

    try {
      // GasLessTransactionData: serialize just the transaction kind (no gas).
      tx.setSenderIfNotSet(account.address);
      const kindBytes = await tx.build({ client, onlyTransactionKind: true });
      const { txBytes, sponsorSignature } = await requestSponsorship(
        account.address,
        toBase64(kindBytes),
      );

      // The wallet co-signs the exact bytes the sponsor signed. Execute over
      // the bytes the wallet returns (identical to txBytes for a fully-built
      // transaction) so the user signature always matches the submitted bytes.
      const { bytes: signedBytes, signature: userSignature } =
        await signTransaction({ transaction: txBytes });

      return await executeSigned(signedBytes, [userSignature, sponsorSignature]);
    } catch (err) {
      // No silent wallet-paid fallback: a sponsorship failure (gas station
      // down, balance too low, or the tx targets a non-allow-listed package)
      // surfaces so the user can turn off the Gas toggle and pay their own gas.
      posthog.captureException(err, { source: "sponsorship_failed" });
      console.warn("sponsorship failed:", err);
      throw new Error(
        "Gas sponsorship failed for this transaction. Turn off the Gas toggle in the header to pay your own gas, then retry.",
      );
    }
  };
}
