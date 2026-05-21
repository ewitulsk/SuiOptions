import type { View } from "../types";

type Props = {
  amount: number;
  setAmount: (n: number) => void;
  view: View;
  btcBalance: number;
  usdcBalance: number;
  error: string;
};

export function AmountInput({
  amount,
  setAmount,
  view,
  btcBalance,
  usdcBalance,
  error,
}: Props) {
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
          <span className="amount__asset-icon">₿</span>
          <div>
            <div className="amount__asset-name">BTC</div>
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
