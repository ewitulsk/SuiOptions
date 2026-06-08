// Unified transaction submit hook.
//
// One entry point for every on-chain write. When the sponsor toggle is on and
// the user is connected, it runs Sui's sponsored-transaction flow: build the
// gasless TransactionKind, get the gas station to attach gas + sign, have the
// wallet co-sign the same bytes, then execute with both signatures. Otherwise
// (toggle off, or sponsorship fails) it falls back to the normal wallet-paid
// `signAndExecuteTransaction`, so the user's action always completes.

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

export function useSubmitTransaction() {
  const account = useCurrentAccount();
  const client = useSuiClient();
  const { mutateAsync: signAndExecute } = useSignAndExecuteTransaction();
  const { mutateAsync: signTransaction } = useSignTransaction();

  return async function submit(tx: Transaction): Promise<void> {
    // Wallet-paid path: toggle off or no connected account.
    if (!isSponsorEnabled() || !account) {
      await signAndExecute({ transaction: tx });
      return;
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

      await client.executeTransactionBlock({
        transactionBlock: signedBytes,
        signature: [userSignature, sponsorSignature],
      });
    } catch (err) {
      // Graceful fallback to wallet-paid: gas station down, balance too low, or
      // the tx targets a non-allow-listed package (e.g. faucet/admin).
      console.warn("sponsorship failed, falling back to wallet-paid:", err);
      await signAndExecute({ transaction: tx });
    }
  };
}
