import type { DashboardModal, DashboardSpots } from "../types";

type Props = {
  modal: DashboardModal;
  spots: DashboardSpots;
  onSubmit: () => void;
  onClose: () => void;
};

export function ActionModal({ modal, spots, onSubmit, onClose }: Props) {
  if (!modal) return null;
  const { stage } = modal;
  const isReview = stage === "review";
  const isSigning = stage === "signing";
  const isBroadcast = stage === "broadcast";
  const isConfirmed = stage === "confirmed";

  if (modal.kind === "exercise") {
    const { position: p, qty } = modal;
    const spot = spots[p.asset] ?? 0;
    const usdcOut = p.strike * qty;
    const usdcValueAtSpot = spot * qty;
    const intrinsic = Math.max(0, (spot - p.strike) * qty);
    return (
      <div className="scrim">
        <div className="modal modal--wide">
          {isReview && (
            <>
              <div className="modal__head-row">
                <div className="modal__kind">exercise · trader</div>
                <button className="modal__x" onClick={onClose}>×</button>
              </div>
              <div className="modal__title">
                Exercise {qty.toFixed(p.asset === "BTC" ? 4 : 0)} {p.asset} call at $
                {p.strike.toLocaleString("en-US")}
              </div>
              <div className="modal__sub">
                tideline · bucket {p.asset.toLowerCase()}_{p.strike}_
                {p.expiry.replaceAll("-", "")}
              </div>
              <div className="modal__list">
                <div className="modal__list-row">
                  <span>strike (you pay)</span>
                  <b>−{usdcOut.toLocaleString("en-US", { maximumFractionDigits: 2 })} USDC</b>
                </div>
                <div className="modal__list-row">
                  <span>{p.asset} received</span>
                  <b>+{qty.toFixed(p.asset === "BTC" ? 4 : 0)} {p.asset}</b>
                </div>
                <div className="modal__list-row">
                  <span>{p.asset} value at spot</span>
                  <b>
                    {usdcValueAtSpot.toLocaleString("en-US", { maximumFractionDigits: 2 })} USDC
                  </b>
                </div>
                <div className="modal__list-row">
                  <span>intrinsic captured</span>
                  <b className="modal__list-val--pos">+{intrinsic.toFixed(2)} USDC</b>
                </div>
                <div className="modal__list-row modal__list-row--quiet">
                  <span>premium paid (sunk)</span>
                  <span>−{p.premiumPaid.toFixed(2)} USDC</span>
                </div>
                <div className="modal__list-row modal__list-row--total">
                  <span>net P/L</span>
                  <b className={p.pnl >= 0 ? "modal__list-val--pos" : "modal__list-val--neg"}>
                    {p.pnl >= 0 ? "+" : "−"}
                    {Math.abs(p.pnl).toFixed(2)} USDC
                  </b>
                </div>
              </div>
              <div className="modal__note">
                Exercise advances the bucket cursor by {qty.toFixed(p.asset === "BTC" ? 4 : 0)}{" "}
                {p.asset}. Your call tokens are burned. The bucket's FIFO assignment picks
                which writer's range gets exercised — you don't pick.
              </div>
              <div className="modal__actions">
                <button className="modal__cta modal__cta--ghost" onClick={onClose}>
                  Cancel
                </button>
                <button className="modal__cta" onClick={onSubmit}>
                  Confirm exercise →
                </button>
              </div>
            </>
          )}
          {isSigning && (
            <>
              <div className="modal__spinner"></div>
              <div className="modal__title">Signing exercise…</div>
              <div className="modal__sub">tideline · waiting for wallet</div>
            </>
          )}
          {isBroadcast && (
            <>
              <div className="modal__spinner"></div>
              <div className="modal__title">Confirming on Sui…</div>
              <div className="modal__sub">advancing bucket cursor</div>
            </>
          )}
          {isConfirmed && (
            <>
              <div className="modal__check">✓</div>
              <div className="modal__title">Exercised.</div>
              <div className="modal__sub">
                {qty.toFixed(p.asset === "BTC" ? 4 : 0)} {p.asset} received
              </div>
              <div className="modal__list">
                <div className="modal__list-row">
                  <span>paid</span>
                  <b>−{usdcOut.toLocaleString("en-US", { maximumFractionDigits: 2 })} USDC</b>
                </div>
                <div className="modal__list-row">
                  <span>received</span>
                  <b>+{qty.toFixed(p.asset === "BTC" ? 4 : 0)} {p.asset}</b>
                </div>
                <div className="modal__list-row">
                  <span>net P/L</span>
                  <b className={p.pnl >= 0 ? "modal__list-val--pos" : "modal__list-val--neg"}>
                    {p.pnl >= 0 ? "+" : "−"}
                    {Math.abs(p.pnl).toFixed(2)} USDC
                  </b>
                </div>
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

  if (modal.kind === "claim") {
    const { position: p } = modal;
    const usdcFromExercise = p.exercisedQty * p.strike;
    const underlyingBack = p.totalQty - p.exercisedQty;
    return (
      <div className="scrim">
        <div className="modal modal--wide">
          {isReview && (
            <>
              <div className="modal__head-row">
                <div className="modal__kind">claim · writer</div>
                <button className="modal__x" onClick={onClose}>×</button>
              </div>
              <div className="modal__title">Claim settlement</div>
              <div className="modal__sub">
                your range [{p.rangeStart.toFixed(3)}, {p.rangeEnd.toFixed(3)}) — bucket
                cursor finished at{" "}
                {(p.cursorAtExpiry ?? p.cursor).toFixed(3)}
              </div>
              <div className="modal__list">
                <div className="modal__list-row">
                  <span>portion exercised</span>
                  <b>
                    {p.exercisedPct.toFixed(0)}% ·{" "}
                    {p.exercisedQty.toFixed(p.asset === "BTC" ? 4 : 0)} {p.asset}
                  </b>
                </div>
                <div className="modal__list-row">
                  <span>USDC from exercise</span>
                  <b className="modal__list-val--pos">
                    +{usdcFromExercise.toLocaleString("en-US", { maximumFractionDigits: 2 })} USDC
                  </b>
                </div>
                <div className="modal__list-row">
                  <span>{p.asset} returned</span>
                  <b className="modal__list-val--pos">
                    +{underlyingBack.toFixed(p.asset === "BTC" ? 4 : 0)} {p.asset}
                  </b>
                </div>
                <div className="modal__list-row modal__list-row--quiet">
                  <span>premium previously received</span>
                  <span>+{p.premiumReceived.toFixed(2)} USDC (already yours)</span>
                </div>
              </div>
              <div className="modal__note">
                Claiming burns your Position NFT and credits both legs to your Account.
                Your premium is unaffected — it landed on day one and is already yours.
              </div>
              <div className="modal__actions">
                <button className="modal__cta modal__cta--ghost" onClick={onClose}>
                  Cancel
                </button>
                <button className="modal__cta" onClick={onSubmit}>
                  Claim →
                </button>
              </div>
            </>
          )}
          {(isSigning || isBroadcast) && (
            <>
              <div className="modal__spinner"></div>
              <div className="modal__title">
                {isSigning ? "Signing claim…" : "Confirming on Sui…"}
              </div>
              <div className="modal__sub">
                {isSigning ? "waiting for wallet" : "burning position NFT"}
              </div>
            </>
          )}
          {isConfirmed && (
            <>
              <div className="modal__check">✓</div>
              <div className="modal__title">Claimed.</div>
              <div className="modal__list">
                <div className="modal__list-row">
                  <span>USDC credited</span>
                  <b className="modal__list-val--pos">
                    +{usdcFromExercise.toLocaleString("en-US", { maximumFractionDigits: 2 })}
                  </b>
                </div>
                <div className="modal__list-row">
                  <span>{p.asset} credited</span>
                  <b className="modal__list-val--pos">
                    +{underlyingBack.toFixed(p.asset === "BTC" ? 4 : 0)}
                  </b>
                </div>
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

  return null;
}
