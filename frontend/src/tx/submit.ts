// Unified transaction submit hook.
//
// One entry point for every on-chain write. When the sponsor toggle is on and
// the user is connected, it runs Sui's sponsored-transaction flow: build the
// gasless TransactionKind, get the gas station to attach gas + sign, have the
// wallet co-sign the same bytes, then execute with both signatures. When the
// toggle is off it goes straight to wallet-paid `signAndExecuteTransaction`.
// Sponsorship failures are NOT silently retried wallet-paid — they surface so
// the user can choose to turn off the Gas toggle and pay their own gas.

import {
  useCurrentAccount,
  useSignAndExecuteTransaction,
  useSignTransaction,
  useSuiClient,
} from "@mysten/dapp-kit";
import type { Transaction } from "@mysten/sui/transactions";
import { toBase64 } from "@mysten/sui/utils";

import { requestSponsorship } from "../api/sponsor";
import { isSponsorEnabled } from "../state/sponsor";
import { posthog } from "../lib/posthog";

export function useSubmitTransaction() {
  const account = useCurrentAccount();
  const client = useSuiClient();
  const { mutateAsync: signAndExecute } = useSignAndExecuteTransaction();
  const { mutateAsync: signTransaction } = useSignTransaction();

  return async function submit(tx: Transaction): Promise<string> {
    // Wallet-paid path: toggle off or no connected account.
    if (!isSponsorEnabled() || !account) {
      const res = await signAndExecute({ transaction: tx });
      return res.digest;
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

      const res = await client.executeTransactionBlock({
        transactionBlock: signedBytes,
        signature: [userSignature, sponsorSignature],
      });
      return res.digest;
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
