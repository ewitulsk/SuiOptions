// Vault action layer: deposit / claim / withdraw. Screens call these handlers;
// they own the submit, the toast, and query invalidation.

import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useCurrentAccount } from "@mysten/dapp-kit";

import type { Vault } from "../api/vaults";
import { useSubmitTransaction } from "../tx/submit";
import {
  buildClaimSharesTx,
  buildCompleteWithdrawTx,
  buildInitiateWithdrawTx,
  buildInstantWithdrawTx,
  buildVaultDepositTx,
  type VaultTypes,
} from "../tx/vault";

export type Toast = { message: string; variant: "success" | "error" };

function typesOf(v: Vault): VaultTypes {
  return {
    underlyingCoinType: v.underlying_coin_type,
    settlementCoinType: v.settlement_coin_type,
    shareType: v.share_type,
  };
}

export function useVaultActions() {
  const account = useCurrentAccount();
  const address = account?.address ?? null;
  const submitTx = useSubmitTransaction();
  const qc = useQueryClient();

  const [busy, setBusy] = useState<string | null>(null);
  const [toast, setToast] = useState<Toast | null>(null);

  // Auto-dismiss so the toast behaves like a notification popup, matching the
  // success/error timings in `state/dashboard.ts`.
  function showToast(next: Toast) {
    setToast(next);
    setTimeout(() => setToast(null), next.variant === "error" ? 6000 : 4500);
  }

  function refresh(vaultId: string) {
    qc.invalidateQueries({ queryKey: ["vault", vaultId] });
    qc.invalidateQueries({ queryKey: ["vaults"] });
    qc.invalidateQueries({ queryKey: ["vault-receipts-owned"] });
    qc.invalidateQueries({ queryKey: ["vault-share-balance"] });
  }

  // One runner for every action: build the wallet PTB and submit it through
  // `submitTx` (sponsor with wallet-paid fallback).
  async function run(
    label: string,
    okMsg: string,
    vault: Vault,
    walletTx: (address: string) => ReturnType<typeof buildVaultDepositTx>,
  ) {
    if (!address) {
      showToast({ message: "Connect a wallet to continue.", variant: "error" });
      return;
    }
    setBusy(label);
    setToast(null);
    try {
      await submitTx(walletTx(address));
      showToast({ message: okMsg, variant: "success" });
      refresh(vault.vault_id);
    } catch (err) {
      showToast({ message: err instanceof Error ? err.message : String(err), variant: "error" });
    } finally {
      setBusy(null);
    }
  }

  return {
    busy,
    toast,
    clearToast: () => setToast(null),

    deposit: (vault: Vault, amountRaw: bigint) =>
      run(
        "depositing",
        "Deposit queued for the next round.",
        vault,
        (address) =>
          buildVaultDepositTx({ vaultId: vault.vault_id, amountRaw, recipient: address, ...typesOf(vault) }),
      ),

    claim: (vault: Vault, receiptId: string) =>
      run(
        "claiming shares",
        "Shares claimed.",
        vault,
        (address) =>
          buildClaimSharesTx({ vaultId: vault.vault_id, receiptId, recipient: address, ...typesOf(vault) }),
      ),

    cancelDeposit: (vault: Vault, receiptId: string) =>
      run(
        "cancelling deposit",
        "Pending deposit refunded.",
        vault,
        (address) =>
          buildInstantWithdrawTx({ vaultId: vault.vault_id, receiptId, recipient: address, ...typesOf(vault) }),
      ),

    initiateWithdraw: (vault: Vault, sharesRaw: bigint) =>
      run(
        "initiating withdrawal",
        "Withdrawal initiated — completes after this round finalizes.",
        vault,
        (address) =>
          buildInitiateWithdrawTx({ vaultId: vault.vault_id, sharesRaw, recipient: address, ...typesOf(vault) }),
      ),

    completeWithdraw: (vault: Vault, receiptId: string) =>
      run(
        "completing withdrawal",
        "Withdrawal paid out.",
        vault,
        (address) =>
          buildCompleteWithdrawTx({ vaultId: vault.vault_id, receiptId, recipient: address, ...typesOf(vault) }),
      ),
  };
}
