import { ChartPanel } from "tideline-frontend";

// Price chart for a bucket's DeepBook pool. Data is fetched live via useBars
// (price-charting REST + WS). In preview there is no live service, so the
// chart frame, series/interval toggles, themed grid, and strike price-line
// render, with the "No quotes yet" empty hint below. Sized box so the
// lightweight-charts canvas has height in headless capture.

// One cell only: without a live price-charting service both pool ids render
// the identical no-data state, so a second variant would just duplicate it.
export const MarketPanel = () => (
  <div style={{ width: 560 }}>
    <ChartPanel
      poolId="0x9c3d1a7e5f0b2c4d6e8f0a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f"
      strike={96000}
      settlementSymbol="USDC"
    />
  </div>
);
