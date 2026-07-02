import { useState } from "react";
import { BucketBar } from "tideline-frontend";

// The top selector rail: asset · instrument · expiry · settlement · live spot.
// The spot price comes from a live Pyth SSE feed. In the preview the `symbol`
// prop is left null so no Hermes EventSource is opened (it would hold the page
// open forever); the price cell then shows its real "connecting…" placeholder,
// while the asset/expiry/settlement pickers render their selected values.

const assets = [
  { symbol: "TBTC", decimals: 8 },
  { symbol: "SUI", decimals: 9 },
  { symbol: "TETH", decimals: 8 },
];

const expiries = [
  { ms: Date.parse("2026-06-26T08:00:00Z"), iso: "2026-06-26T08:00:00Z" },
  { ms: Date.parse("2026-07-31T08:00:00Z"), iso: "2026-07-31T08:00:00Z" },
  { ms: Date.parse("2026-09-25T08:00:00Z"), iso: "2026-09-25T08:00:00Z" },
];

export const BtcSeries = () => {
  const [asset, setAsset] = useState<string | null>("TBTC");
  const [expiry, setExpiry] = useState<number | null>(expiries[0].ms);
  return (
    <BucketBar
      symbol={null}
      assets={assets}
      selectedAsset={asset}
      onSelectAsset={setAsset}
      expiries={expiries}
      selectedExpiryMs={expiry}
      onSelectExpiry={setExpiry}
      settlementSymbol="USDC"
    />
  );
};

export const SuiSeries = () => {
  const [asset, setAsset] = useState<string | null>("SUI");
  const [expiry, setExpiry] = useState<number | null>(expiries[1].ms);
  return (
    <BucketBar
      symbol={null}
      assets={assets}
      selectedAsset={asset}
      onSelectAsset={setAsset}
      expiries={expiries}
      selectedExpiryMs={expiry}
      onSelectExpiry={setExpiry}
      settlementSymbol="USDC"
    />
  );
};
