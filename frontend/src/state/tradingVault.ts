// Trading-vault action layer: create / deposit / request-withdraw plus the
// v2 position lifecycle (split/merge/transfer/burn), settlement claims, and
// the curator capital ops (SO-418). Screens call these handlers; they own
// the submit, the toast, and query invalidation. Mirrors `state/vault.ts`.
//
// Sponsorship map (must match sui-tx template.rs — any PTB shape without a
// template silently isn't sponsored):
//   sponsored: deposit (incl. the trailing position transfer),
//     request_withdraw (owned-object input), split-then-withdraw,
//     amend_payout_asset, split, merge, redeem_settled_position,
//     settle_queued_request.
//   wallet-paid: create_vault and every curator op (commitment funding /
//     release, reset propose/execute, settlement snapshot, fee claims,
//     asset mgmt, external venue), plus plain position transfers and
//     wiped-position burns (no protocol template).

import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useCurrentAccount } from "@mysten/dapp-kit";
import type { Transaction } from "@mysten/sui/transactions";

import { useSubmitTransaction } from "../tx/submit";
import {
  buildAppraisedDepositTx,
  buildCrankCapitalTx,
  buildDepositIntoCommitmentTx,
  buildExecuteJuniorResetTx,
  buildProposeJuniorResetTx,
  buildReleaseCommitmentTx,
  buildReleaseExternalTx,
  buildSnapshotSettlementTx,
  type AppraisedCrankParams,
  type AppraisedDepositParams,
  type DepositIntoCommitmentParams,
  type ExecuteJuniorResetParams,
  type ReleaseCommitmentParams,
  type ReleaseExternalParams,
} from "../tx/appraisal";
import {
  buildExchangeAddSignerTx,
  buildExchangeDefundTx,
  buildExchangeFundTx,
  buildExchangeRemoveSignerTx,
  buildInitExchangeCustodyTx,
  type ExchangeCustodyMoveParams,
  type ExchangeSignerParams,
  type InitExchangeCustodyParams,
} from "../tx/exchangeAdapter";
import {
  buildAddDepositAssetTx,
  buildAmendPayoutAssetTx,
  buildBurnWipedPositionTx,
  buildClaimSettlementCuratorFeesTx,
  buildCreateTradingVaultTx,
  buildCuratorTakerSwapTx,
  buildMergePositionsTx,
  buildRedeemSettledPositionTx,
  buildRemoveDepositAssetTx,
  buildSetHaircutsTx,
  buildSettleQueuedRequestTx,
  buildSplitPositionTx,
  buildSplitThenWithdrawTx,
  buildTradingVaultDepositTx,
  buildTradingVaultWithdrawTx,
  buildTransferPositionTx,
  type AmendPayoutAssetParams,
  type BurnWipedPositionParams,
  type ClaimSettlementCuratorFeesParams,
  type CreateTradingVaultParams,
  type CuratorTakerSwapParams,
  type DepositAssetParams,
  type MergePositionsParams,
  type RedeemSettledPositionParams,
  type SetHaircutsParams,
  type SettleQueuedRequestParams,
  type SplitPositionParams,
  type SplitThenWithdrawParams,
  type TradingVaultDepositParams,
  type TradingVaultWithdrawParams,
  type TransferPositionParams,
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
    // Allowlist + pending withdrawal lanes, read from chain (SO-370/418).
    qc.invalidateQueries({ queryKey: ["trading-vault-onchain"] });
    // Custodied positions + exchange BM state (SO-373).
    qc.invalidateQueries({ queryKey: ["trading-vault-holdings"] });
    qc.invalidateQueries({ queryKey: ["trading-vault-exchange-bm"] });
    // v2 read model: wallet positions, waterfall, settlement, lanes (SO-418).
    qc.invalidateQueries({ queryKey: ["trading-vault-positions"] });
    qc.invalidateQueries({ queryKey: ["trading-vault-position-detail"] });
    qc.invalidateQueries({ queryKey: ["trading-vault-waterfall"] });
    qc.invalidateQueries({ queryKey: ["trading-vault-settlement"] });
    qc.invalidateQueries({ queryKey: ["trading-vault-pending-requests"] });
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

    // Vault creation is a curator act (the creator IS the curator) — never
    // gas-sponsored (SO-418).
    createVault: (params: CreateTradingVaultParams) =>
      run(
        "creating vault",
        "Vault created — it will appear once indexed.",
        () => buildCreateTradingVaultTx(params),
        { sponsor: false },
      ),

    deposit: (params: TradingVaultDepositParams) =>
      run(
        "depositing",
        "Deposit complete — position NFT minted to your wallet.",
        () => buildTradingVaultDepositTx(params),
      ),

    // SO-289: appraisal-composed deposit — values every held asset and
    // custodied position in the same PTB so `deposit` sees a complete NAV.
    // Non-accounting deposits (SO-370) carry their own attestation and ride
    // the unsponsored path — wallet-paid, like every attestation-bearing
    // curator op.
    depositAppraised: (params: AppraisedDepositParams) =>
      run(
        "depositing",
        "Deposit complete — position NFT minted to your wallet.",
        () => buildAppraisedDepositTx(params),
        params.plan.depositAssetType !== params.plan.accountingType
          ? { sponsor: false }
          : undefined,
      ),

    // v2: consumes the WHOLE position object.
    requestWithdraw: (params: TradingVaultWithdrawParams) =>
      run(
        "requesting withdrawal",
        "Withdrawal queued — paid out lane-FIFO as the curator frees funds.",
        () => buildTradingVaultWithdrawTx(params),
      ),

    // v2 partial exit: split the requested shares off, queue the child.
    splitThenWithdraw: (params: SplitThenWithdrawParams) =>
      run(
        "requesting withdrawal",
        "Position split — the withdrawn part is queued, the rest stays in your wallet.",
        () => buildSplitThenWithdrawTx(params),
      ),

    // SO-370: recipient re-points a pending request's payout asset.
    amendPayoutAsset: (params: AmendPayoutAssetParams) =>
      run(
        "amending payout",
        "Payout asset amended.",
        () => buildAmendPayoutAssetTx(params),
      ),

    // ── v2 position lifecycle (SO-418) ──
    splitPosition: (params: SplitPositionParams) =>
      run(
        "splitting position",
        "Position split — both parts are in your wallet.",
        () => buildSplitPositionTx(params),
      ),

    mergePositions: (params: MergePositionsParams) =>
      run(
        "merging positions",
        "Positions merged — shares and basis added, the later lock wins.",
        () => buildMergePositionsTx(params),
      ),

    // Plain object transfer — no protocol template, wallet-paid. The UI
    // interposes the value-vs-basis disclosure before calling this.
    transferPosition: (params: TransferPositionParams) =>
      run(
        "transferring position",
        "Position transferred.",
        () => buildTransferPositionTx(params),
        { sponsor: false },
      ),

    burnWipedPosition: (params: BurnWipedPositionParams) =>
      run(
        "burning position",
        "Wiped position burned.",
        () => buildBurnWipedPositionTx(params),
        { sponsor: false },
      ),

    // ── settlement claims (SO-418) — sponsored: users exiting a closed
    // vault shouldn't need gas. ──
    redeemSettledPosition: (params: RedeemSettledPositionParams) =>
      run(
        "redeeming",
        "Position redeemed against the settlement pool.",
        () => buildRedeemSettledPositionTx(params),
      ),

    settleQueuedRequest: (params: SettleQueuedRequestParams) =>
      run(
        "settling request",
        "Queued request settled from the pool.",
        () => buildSettleQueuedRequestTx(params),
      ),

    // ── curator capital ops (SO-418) — never gas-sponsored. ──
    depositIntoCommitment: (params: DepositIntoCommitmentParams) =>
      run(
        "funding commitment",
        "Commitment funded — the escrowed position grew.",
        () => buildDepositIntoCommitmentTx(params),
        { sponsor: false },
      ),

    releaseCommitment: (params: ReleaseCommitmentParams) =>
      run(
        "releasing commitment",
        "Commitment released to your wallet as a position NFT.",
        () => buildReleaseCommitmentTx(params),
        { sponsor: false },
      ),

    proposeJuniorReset: (params: AppraisedCrankParams) =>
      run(
        "proposing reset",
        "Junior reset proposed — executable after the notice period.",
        () => buildProposeJuniorResetTx(params),
        { sponsor: false },
      ),

    executeJuniorReset: (params: ExecuteJuniorResetParams) =>
      run(
        "executing reset",
        "Junior reset executed — you hold the new generation's genesis position.",
        () => buildExecuteJuniorResetTx(params),
        { sponsor: false },
      ),

    snapshotSettlement: (params: AppraisedCrankParams) =>
      run(
        "snapshotting settlement",
        "Settlement snapshot taken — entitlements are frozen.",
        () => buildSnapshotSettlementTx(params),
        { sponsor: false },
      ),

    crankCapital: (params: AppraisedCrankParams) =>
      run(
        "syncing capital",
        "Capital state synced.",
        () => buildCrankCapitalTx(params),
        { sponsor: false },
      ),

    claimSettlementCuratorFees: (params: ClaimSettlementCuratorFeesParams) =>
      run(
        "claiming fees",
        "Settlement curator fees claimed.",
        () => buildClaimSettlementCuratorFeesTx(params),
        { sponsor: false },
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

    // SO-370 curator allowlist + haircut management — wallet-paid.
    addDepositAsset: (params: DepositAssetParams) =>
      run(
        "allowing asset",
        "Deposit asset added to the allowlist.",
        () => buildAddDepositAssetTx(params),
        { sponsor: false },
      ),

    removeDepositAsset: (params: Omit<DepositAssetParams, "protocolConfigId">) =>
      run(
        "removing asset",
        "Deposit asset removed from the allowlist.",
        () => buildRemoveDepositAssetTx(params),
        { sponsor: false },
      ),

    setHaircuts: (params: SetHaircutsParams) =>
      run(
        "setting haircuts",
        "Entry/exit haircuts updated.",
        () => buildSetHaircutsTx(params),
        { sponsor: false },
      ),

    // SO-373 exchange custody (curator, wallet-paid): create the cap-owned
    // BalanceManager, swap capital in/out, delegate order signers.
    initExchangeCustody: (params: InitExchangeCustodyParams) =>
      run(
        "creating custody",
        "Exchange custody created.",
        () => buildInitExchangeCustodyTx(params),
        { sponsor: false },
      ),

    exchangeFund: (params: ExchangeCustodyMoveParams) =>
      run(
        "funding",
        "Moved into the exchange balance manager.",
        () => buildExchangeFundTx(params),
        { sponsor: false },
      ),

    exchangeDefund: (params: ExchangeCustodyMoveParams) =>
      run(
        "defunding",
        "Moved back into vault free balances.",
        () => buildExchangeDefundTx(params),
        { sponsor: false },
      ),

    exchangeAddSigner: (params: ExchangeSignerParams) =>
      run(
        "adding signer",
        "Order signer delegated.",
        () => buildExchangeAddSignerTx(params),
        { sponsor: false },
      ),

    exchangeRemoveSigner: (params: ExchangeSignerParams) =>
      run(
        "removing signer",
        "Order signer removed — its resting orders are void.",
        () => buildExchangeRemoveSignerTx(params),
        { sponsor: false },
      ),
  };
}
