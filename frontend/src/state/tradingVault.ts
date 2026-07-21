// Trading-vault action layer: create / deposit / request-withdraw. Screens
// call these handlers; they own the submit, the toast, and query invalidation.
// Mirrors `state/vault.ts`.

import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useCurrentAccount } from "@mysten/dapp-kit";
import type { Transaction } from "@mysten/sui/transactions";

import { useSubmitTransaction } from "../tx/submit";
import {
  buildAppraisedDepositTx,
  buildReleaseExternalTx,
  type AppraisedDepositParams,
  type ReleaseExternalParams,
} from "../tx/appraisal";
import {
  buildCreateTradingVaultTx,
  buildCuratorTakerSwapTx,
  buildTradingVaultDepositTx,
  buildTradingVaultWithdrawTx,
  type CreateTradingVaultParams,
  type CuratorTakerSwapParams,
  type TradingVaultDepositParams,
  type TradingVaultWithdrawParams,
} from "../tx/tradingVault";

export type Toast = { message: string; variant: "success" | "error" };

export function useTradingVaultActions() {
  const account = useCurrentAccount();
  const address = account?.address ?? null;
  const submitTx = useSubmitTransaction();
  const qc = useQueryClient();

  const [busy, setBusy] = useState<string | null>(null);
  const [toast, setToast] = useState<Toast | null>(null);

  // Auto-dismiss so the toast behaves like a notification popup, matching the
  // success/error timings in `state/vault.ts`.
  function showToast(next: Toast) {
    setToast(next);
    setTimeout(() => setToast(null), next.variant === "error" ? 6000 : 4500);
  }

  function refresh() {
    qc.invalidateQueries({ queryKey: ["trading-vaults"] });
    qc.invalidateQueries({ queryKey: ["trading-vault"] });
    qc.invalidateQueries({ queryKey: ["coin-balance"] });
  }

  // One runner for every action: build the wallet PTB and submit it through
  // `submitTx` (sponsored when the Gas toggle is on). Builders may be async —
  // the appraised deposit fetches a Hermes price update while building.
  async function run(
    label: string,
    okMsg: string,
    buildTx: () => Transaction | Promise<Transaction>,
    opts?: { sponsor?: boolean },
  ) {
    if (!address) {
      showToast({ message: "Connect a wallet to continue.", variant: "error" });
      return;
    }
    setBusy(label);
    setToast(null);
    try {
      await submitTx(await buildTx(), opts);
      showToast({ message: okMsg, variant: "success" });
      refresh();
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

    createVault: (params: CreateTradingVaultParams) =>
      run(
        "creating vault",
        "Vault created — it will appear once indexed.",
        () => buildCreateTradingVaultTx(params),
      ),

    deposit: (params: TradingVaultDepositParams) =>
      run(
        "depositing",
        "Deposit complete — shares minted at NAV.",
        () => buildTradingVaultDepositTx(params),
      ),

    // SO-289: appraisal-composed deposit — values every held asset and
    // custodied position in the same PTB so `deposit` sees a complete NAV.
    depositAppraised: (params: AppraisedDepositParams) =>
      run(
        "depositing",
        "Deposit complete — shares minted at NAV.",
        () => buildAppraisedDepositTx(params),
      ),

    requestWithdraw: (params: TradingVaultWithdrawParams) =>
      run(
        "requesting withdrawal",
        "Withdrawal queued — paid out FIFO as the curator frees funds.",
        () => buildTradingVaultWithdrawTx(params),
      ),

    // Curator ops (SO-299) are never gas-sponsored — always wallet-paid.
    releaseExternal: (params: ReleaseExternalParams) =>
      run(
        "releasing",
        "Released to the external account.",
        () => buildReleaseExternalTx(params),
        { sponsor: false },
      ),

    spotSwap: (params: CuratorTakerSwapParams) =>
      run(
        "swapping",
        "Swap executed against vault free balances.",
        () => buildCuratorTakerSwapTx(params),
        { sponsor: false },
      ),
  };
}
