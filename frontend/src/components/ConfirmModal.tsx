import { formatPrice } from "../format";
import type { ConfirmStage, ConfirmSummary, OptionType, View } from "../types";

type Props = {
  stage: ConfirmStage;
  summary: ConfirmSummary | null;
  view: View;
  optionType: OptionType;
  onClose: () => void;
};

export function ConfirmModal({ stage, summary, view, optionType, onClose }: Props) {
  const isPut = optionType === "put";
  return (
    <div className="scrim">
      <div className="modal">
        {stage === "signing" && (
          <>
            <div className="modal__spinner"></div>
            <div className="modal__title">Signing quote…</div>
            <div className="modal__sub">pismo protocol · waiting for wallet</div>
          </>
        )}
        {stage === "broadcast" && (
          <>
            <div className="modal__spinner"></div>
            <div className="modal__title">Confirming on Sui…</div>
            <div className="modal__sub">broadcasting transaction</div>
          </>
        )}
        {stage === "confirmed" && summary && (
          <>
            <div className="modal__check">✓</div>
            <div className="modal__title">
              {view === "writer"
                ? "Position opened."
                : isPut
                  ? "Put purchased."
                  : "Call purchased."}
            </div>
            <div className="modal__sub">{summary.bucket}</div>
            <div className="modal__list">
              {view === "writer" ? (
                <>
                  <div className="modal__list-row">
                    <span>premium received</span>
                    <b>+{formatPrice(summary.premium)} USDC</b>
                  </div>
                  <div className="modal__list-row">
                    <span>your range</span>
                    <b>
                      [{summary.rangeStart.toFixed(3)}, {summary.rangeEnd.toFixed(3)})
                    </b>
                  </div>
                  <div className="modal__list-row">
                    <span>{isPut ? "cash collateral locked" : "collateral locked"}</span>
                    <b>
                      {isPut
                        ? `${formatPrice(summary.amount * summary.strike, { grouping: true })} USDC`
                        : `${summary.amount.toFixed(4)} ${summary.asset}`}
                    </b>
                  </div>
                </>
              ) : (
                <>
                  <div className="modal__list-row">
                    <span>{isPut ? "put options minted" : "call options minted"}</span>
                    <b>
                      {summary.amount.toFixed(4)} {summary.asset} @ ${formatPrice(summary.strike, { grouping: true })}
                    </b>
                  </div>
                  <div className="modal__list-row">
                    <span>premium paid</span>
                    <b>−{formatPrice(summary.premium)} USDC</b>
                  </div>
                  <div className="modal__list-row">
                    <span>expiry</span>
                    <b>{summary.expiry}</b>
                  </div>
                </>
              )}
            </div>
            <button className="modal__cta" onClick={onClose}>
              Done
            </button>
          </>
        )}
      </div>
    </div>
  );
}
