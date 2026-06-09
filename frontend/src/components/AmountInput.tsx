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
  return (
    <div>
      <div className="amount">
        <div className="amount__field">
          <input
            className="amount__input"
            type="number"
            min="0"
            step="0.001"
            value={amount}
            onChange={(e) => setAmount(parseFloat(e.target.value) || 0)}
          />
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
