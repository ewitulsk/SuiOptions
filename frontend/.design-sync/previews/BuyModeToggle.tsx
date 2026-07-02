import { useState } from "react";
import { BuyModeToggle } from "tideline-frontend";
import type { BuyMode } from "tideline-frontend";

// Segmented control for the Buy screen's two purchase paths: trading on
// DeepBook vs. minting from the market makers. A blue pill slides between them.

export const DeepBook = () => {
  const [mode, setMode] = useState<BuyMode>("deepbook");
  return <BuyModeToggle mode={mode} onChange={setMode} />;
};

export const MarketMakers = () => {
  const [mode, setMode] = useState<BuyMode>("mm");
  return <BuyModeToggle mode={mode} onChange={setMode} />;
};
