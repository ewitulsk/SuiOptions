import { ConfirmModal } from "tideline-frontend";
import type { ConfirmSummary } from "../../src/types";

// Post-trade confirmation overlay from the Composer. Spinner stages have no
// summary; the confirmed stage renders a per-view breakdown list.
// The modal scrim is position:fixed; the preview provider's transform wrapper
// makes "fixed" relative to itself, so this in-flow spacer gives the scrim
// (and the centered modal) full height instead of collapsing.
const Frame = ({ children }: { children: React.ReactNode }) => (
  <div style={{ height: 680 }}>{children}</div>
);

const writerSummary: ConfirmSummary = {
  view: "writer",
  premium: 314.2,
  bucket: "btc_100000_20260731",
  rangeStart: 1.2,
  rangeEnd: 1.3,
  amount: 0.1,
  strike: 100000,
  asset: "BTC",
  expiry: "2026-07-31",
};

const traderSummary: ConfirmSummary = {
  view: "trader",
  premium: 199.0,
  bucket: "btc_100000_20260731",
  rangeStart: 1.2,
  rangeEnd: 1.3,
  amount: 0.05,
  strike: 100000,
  asset: "BTC",
  expiry: "2026-07-31",
};

// Wallet signing spinner stage.
export const Signing = () => (
  <Frame><ConfirmModal stage="signing" summary={null} view="trader" onClose={() => {}} /></Frame>
);

// On-chain broadcast spinner stage.
export const Broadcast = () => (
  <Frame><ConfirmModal stage="broadcast" summary={null} view="trader" onClose={() => {}} /></Frame>
);

// Writer success: premium received / range / collateral locked.
export const WriterConfirmed = () => (
  <Frame>
    <ConfirmModal stage="confirmed" summary={writerSummary} view="writer" onClose={() => {}} />
  </Frame>
);

// Trader success: call options minted / premium paid / expiry.
export const TraderConfirmed = () => (
  <Frame>
    <ConfirmModal stage="confirmed" summary={traderSummary} view="trader" onClose={() => {}} />
  </Frame>
);
