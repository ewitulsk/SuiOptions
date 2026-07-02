import { useState } from "react";
import { AmountInput } from "tideline-frontend";

// Quantity field with a custom stepper, Max button, and the asset disc.
// (Token logos come from the live token-info catalog, which is absent in the
// preview, so the disc shows the asset's initial glyph — the real fallback.)

// Writer side: enter the underlying quantity; balance shown in the underlying.
export const Writer = () => {
  const [amount, setAmount] = useState(0.05);
  return (
    <AmountInput
      amount={amount}
      setAmount={setAmount}
      view="writer"
      assetSymbol="TBTC"
      btcBalance={1.2345}
      usdcBalance={5000}
      spot={96000}
      settlementSymbol="USDC"
      error=""
    />
  );
};

// Trader side: the asset⇄USDC denomination toggle is enabled (spot > 0).
export const Trader = () => {
  const [amount, setAmount] = useState(0.1);
  return (
    <AmountInput
      amount={amount}
      setAmount={setAmount}
      view="trader"
      assetSymbol="TBTC"
      btcBalance={1.2}
      usdcBalance={5000}
      spot={96000}
      settlementSymbol="USDC"
      error=""
    />
  );
};

// Validation state: the error line under the field.
export const InsufficientBalance = () => {
  const [amount, setAmount] = useState(0.5);
  return (
    <AmountInput
      amount={amount}
      setAmount={setAmount}
      view="trader"
      assetSymbol="TBTC"
      btcBalance={0.1}
      usdcBalance={50}
      spot={96000}
      settlementSymbol="USDC"
      error="INSUFFICIENT USDC · NEED 9600.00"
    />
  );
};
