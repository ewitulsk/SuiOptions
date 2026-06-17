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
  /** Live spot price (settlement per 1 underlying). Powers the USDC-denominated
   *  input on the Buy page; 0/absent disables the toggle. */
  spot: number;
  /** Settlement-asset symbol (e.g. `USDC`) for the alternate denomination. */
  settlementSymbol: string;
};

type Denom = "underlying" | "stable";

function initialOf(symbol: string | null): string {
  return (symbol ?? "").replace(/[^A-Za-z0-9]/g, "").charAt(0).toUpperCase() || "?";
}

export function AmountInput({
  amount,
  setAmount,
  view,
  assetSymbol,
  btcBalance,
  usdcBalance,
  error,
  spot,
  settlementSymbol,
}: Props) {
  // USDC-denominated entry is a Buy-page convenience: type the underlying
  // quantity as its spot-notional value instead. `amount` (underlying display
  // units) stays the source of truth for everything downstream.
  const [denom, setDenom] = useState<Denom>("underlying");
  const canDenominate = view === "trader" && spot > 0;
  const stableMode = canDenominate && denom === "stable";

  // Display ⇄ underlying conversion. In stable mode the field shows a USDC
  // figure (underlying × spot); typing divides back out by spot.
  const toDisplay = (u: number) => (stableMode ? u * spot : u);
  const fromDisplay = (d: number) => (stableMode ? d / spot : d);

  const unitSymbol = stableMode ? settlementSymbol : assetSymbol;
  const unitName = findToken(unitSymbol)?.name ?? unitSymbol ?? "—";
  const unitInitial = initialOf(unitSymbol);

  // Mirror the input as a raw string so it can be fully cleared. We re-sync the
  // text from `amount` only when an *external* write lands (Max, stepper, denom
  // switch) — tracked via `synced`, the (amount, stableMode) pair the current
  // text already represents. Comparing this pair rather than re-deriving the
  // numeric value avoids float round-trip jitter (and the render loop a
  // tolerance check would risk).
  const trim = (n: number) => String(Number(n.toPrecision(12)));
  const [text, setText] = useState(() => (amount === 0 ? "" : trim(toDisplay(amount))));
  const [synced, setSynced] = useState({ amount, stableMode });
  if (synced.amount !== amount || synced.stableMode !== stableMode) {
    setText(amount === 0 ? "" : trim(toDisplay(amount)));
    setSynced({ amount, stableMode });
  }

  const update = (raw: string) => {
    setText(raw);
    const u = fromDisplay(Number(raw) || 0);
    setAmount(u);
    // Mark this amount as already-reflected so the render-sync above leaves the
    // user's in-progress text (e.g. "0." or "5000") untouched.
    setSynced({ amount: u, stableMode });
  };

  // Custom stepper replaces the native number spinner (hidden in CSS). Steps in
  // the active denomination ($10 in USDC mode, 0.001 underlying otherwise) and
  // clamps at 0; the render-sync pushes the new value back into `text`.
  const step = (dir: 1 | -1) => {
    if (stableMode) {
      const nextUsd = Math.max(0, Number((toDisplay(amount) + dir * 10).toFixed(2)));
      setAmount(fromDisplay(nextUsd));
    } else {
      setAmount(Math.max(0, Number((amount + dir * 0.001).toFixed(3))));
    }
  };

  // Balance shown is the side's spending balance: USDC for the trader (in either
  // denomination), the underlying for the writer.
  const balLine =
    view === "writer" ? btcBalance.toFixed(4) : `${usdcBalance.toFixed(2)} USDC`;
  // The equivalent value in the other unit, so both numbers are always visible.
  const equivLine = stableMode
    ? `≈ ${amount.toFixed(4)} ${assetSymbol ?? ""}`.trim()
    : canDenominate
      ? `≈ ${(amount * spot).toFixed(2)} ${settlementSymbol}`
      : null;

  const meta = (
    <div className="amount__asset-meta">
      <div className="amount__asset-name">{unitName}</div>
      <div className="amount__asset-bal">bal {balLine}</div>
      {equivLine && <div className="amount__asset-equiv">{equivLine}</div>}
    </div>
  );

  // One token disc. The active denomination renders large/opaque, the swap
  // target small/faded; both are always mounted so toggling `denom` just flips
  // the active/alt classes and CSS animates the size+position swap.
  const coin = (symbol: string | null, active: boolean) => {
    const cls = "amount__coin " + (active ? "amount__coin--active" : "amount__coin--alt");
    return (
      <TokenLogo
        symbol={symbol}
        className={cls}
        fallback={
          <span className={cls + " amount__asset-icon--generic"}>{initialOf(symbol)}</span>
        }
      />
    );
  };

  return (
    <div>
      <div className="amount">
        <div className="amount__field">
          <input
            className="amount__input"
            type="number"
            min="0"
            step={stableMode ? "0.01" : "0.001"}
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
        {canDenominate ? (
          <button
            type="button"
            className="amount__asset amount__asset--toggle"
            onClick={() => setDenom((d) => (d === "stable" ? "underlying" : "stable"))}
            aria-label={`Switch input to ${stableMode ? assetSymbol ?? "underlying" : settlementSymbol}`}
            title={`Type the amount in ${stableMode ? assetSymbol ?? "the underlying" : settlementSymbol} instead`}
          >
            <span className="amount__coins" aria-hidden="true">
              {coin(assetSymbol, !stableMode)}
              {coin(settlementSymbol, stableMode)}
            </span>
            {meta}
          </button>
        ) : (
          <div className="amount__asset">
            <TokenLogo
              symbol={unitSymbol}
              className="amount__asset-icon"
              fallback={
                <span className="amount__asset-icon amount__asset-icon--generic">
                  {unitInitial}
                </span>
              }
            />
            {meta}
          </div>
        )}
      </div>
      <div className="amount__error">{error}</div>
    </div>
  );
}
