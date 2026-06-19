// Vault action layer: deposit / claim / withdraw, routed to the wallet or the
// session-login custody path exactly like `state/dashboard.ts` routes
// exercise/claim. Screens call these handlers; they own the wallet-vs-session
// branch, the submit, the toast, and query invalidation.

import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";

import type { Vault } from "../api/vaults";
import { useUserIdentity } from "../session/identity";
import { executeWithSession } from "../session/store";
import { useSubmitTransaction } from "../tx/submit";
import {
  buildClaimSharesTx,
  buildCompleteWithdrawTx,
  buildInitiateWithdrawTx,
  buildInstantWithdrawTx,
  buildVaultDepositTx,
  type VaultTypes,
} from "../tx/vault";
import {
  addSessionVaultClaimShares,
  addSessionVaultCompleteWithdraw,
  addSessionVaultDeposit,
  addSessionVaultInitiateWithdraw,
  addSessionVaultInstantWithdraw,
} from "../tx/session";

export type Toast = { message: string; variant: "success" | "error" };

function typesOf(v: Vault): VaultTypes {
  return {
    underlyingCoinType: v.underlying_coin_type,
    settlementCoinType: v.settlement_coin_type,
    shareType: v.share_type,
  };
}

export function useVaultActions() {
  const identity = useUserIdentity();
  const sessionState = identity?.kind === "session" ? identity.session : null;
  const address = identity?.address ?? null;
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

  // One runner for both paths: session twins go through `executeWithSession`
  // (always sponsored, custody-funded); wallet PTBs through `submitTx` (sponsor
  // with wallet-paid fallback).
  async function run(
    label: string,
    okMsg: string,
    vault: Vault,
    walletTx: (address: string) => ReturnType<typeof buildVaultDepositTx>,
    sessionAdd: (tx: Parameters<typeof addSessionVaultDeposit>[0], ctx: Parameters<typeof addSessionVaultDeposit>[1]) => void,
  ) {
    if (!address) {
      showToast({ message: "Connect a wallet or sign in to continue.", variant: "error" });
      return;
    }
    setBusy(label);
    setToast(null);
    try {
      if (sessionState) {
        await executeWithSession(label, (tx, ctx) => sessionAdd(tx, ctx));
      } else {
        await submitTx(walletTx(address));
      }
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
        (tx, ctx) =>
          addSessionVaultDeposit(tx, ctx, { vaultId: vault.vault_id, amountRaw, ...typesOf(vault) }),
      ),

    claim: (vault: Vault, receiptId: string) =>
      run(
        "claiming shares",
        "Shares claimed.",
        vault,
        (address) =>
          buildClaimSharesTx({ vaultId: vault.vault_id, receiptId, recipient: address, ...typesOf(vault) }),
        (tx, ctx) =>
          addSessionVaultClaimShares(tx, ctx, { vaultId: vault.vault_id, receiptId, ...typesOf(vault) }),
      ),

    cancelDeposit: (vault: Vault, receiptId: string) =>
      run(
        "cancelling deposit",
        "Pending deposit refunded.",
        vault,
        (address) =>
          buildInstantWithdrawTx({ vaultId: vault.vault_id, receiptId, recipient: address, ...typesOf(vault) }),
        (tx, ctx) =>
          addSessionVaultInstantWithdraw(tx, ctx, { vaultId: vault.vault_id, receiptId, ...typesOf(vault) }),
      ),

    initiateWithdraw: (vault: Vault, sharesRaw: bigint) =>
      run(
        "initiating withdrawal",
        "Withdrawal initiated — completes after this round finalizes.",
        vault,
        (address) =>
          buildInitiateWithdrawTx({ vaultId: vault.vault_id, sharesRaw, recipient: address, ...typesOf(vault) }),
        (tx, ctx) =>
          addSessionVaultInitiateWithdraw(tx, ctx, { vaultId: vault.vault_id, sharesRaw, ...typesOf(vault) }),
      ),

    completeWithdraw: (vault: Vault, receiptId: string) =>
      run(
        "completing withdrawal",
        "Withdrawal paid out.",
        vault,
        (address) =>
          buildCompleteWithdrawTx({ vaultId: vault.vault_id, receiptId, recipient: address, ...typesOf(vault) }),
        (tx, ctx) =>
          addSessionVaultCompleteWithdraw(tx, ctx, { vaultId: vault.vault_id, receiptId, ...typesOf(vault) }),
      ),
  };
}
