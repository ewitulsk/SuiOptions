import { useState } from "react";
import type { View } from "../types";
import { findToken } from "../config";
import { TokenLogo } from "./TokenLogo";

type Props = {
  amount: number;
  setAmount: (n: number) => void;
  view: View;
  /** Underlying-asset symbol (on-chain ticker, e.g. `TBTC`); null while loading. */
  assetSymbol: string | null;
  btcBalance: number;
  usdcBalance: number;
  error: string;
};

export function AmountInput({
  amount,
  setAmount,
  view,
  assetSymbol,
  btcBalance,
  usdcBalance,
  error,
}: Props) {
  const assetName = findToken(assetSymbol)?.name ?? assetSymbol ?? "—";
  const assetInitial =
    (assetSymbol ?? "").replace(/[^A-Za-z0-9]/g, "").charAt(0).toUpperCase() || "?";

  // Mirror the input as a raw string so it can be fully cleared. The numeric
  // `amount` stays the source of truth for everything downstream; an empty (or
  // partial, e.g. "0.") field maps to 0. Re-sync only when an external write
  // (e.g. Max) lands a value that the current text doesn't already represent.
  const [text, setText] = useState(() => (amount === 0 ? "" : String(amount)));
  if ((Number(text) || 0) !== amount) {
    setText(amount === 0 ? "" : String(amount));
  }

  const update = (raw: string) => {
    setText(raw);
    setAmount(Number(raw) || 0);
  };

  // Custom stepper replaces the native number spinner (hidden in CSS). Steps by
  // the input's `step` and clamps at 0; the render-sync above pushes the new
  // value back into `text`.
  const STEP = 0.001;
  const step = (dir: 1 | -1) => {
    setAmount(Math.max(0, Number((amount + dir * STEP).toFixed(3))));
  };

  return (
    <div>
      <div className="amount">
        <div className="amount__field">
          <input
            className="amount__input"
            type="number"
            min="0"
            step="0.001"
            value={text}
            onChange={(e) => update(e.target.value)}
          />
          <div className="amount__stepper">
            <button
              type="button"
              className="amount__step"
              aria-label="Increase amount"
              onClick={() => step(1)}
            >
              <svg width="10" height="6" viewBox="0 0 10 6" aria-hidden="true">
                <path d="M1 5L5 1L9 5" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" />
              </svg>
            </button>
            <button
              type="button"
              className="amount__step"
              aria-label="Decrease amount"
              onClick={() => step(-1)}
            >
              <svg width="10" height="6" viewBox="0 0 10 6" aria-hidden="true">
                <path d="M1 1L5 5L9 1" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" />
              </svg>
            </button>
          </div>
          <button
            className="amount__max"
            onClick={() => setAmount(view === "writer" ? btcBalance : 0.1)}
          >
            Max
          </button>
        </div>
        <div className="amount__asset">
          <TokenLogo
            symbol={assetSymbol}
            className="amount__asset-icon"
            fallback={
              <span className="amount__asset-icon amount__asset-icon--generic">
                {assetInitial}
              </span>
            }
          />
          <div>
            <div className="amount__asset-name">{assetName}</div>
            <div className="amount__asset-bal">
              bal {view === "writer" ? btcBalance.toFixed(4) : `${usdcBalance.toFixed(2)} USDC`}
            </div>
          </div>
        </div>
      </div>
      <div className="amount__error">{error}</div>
    </div>
  );
}
