import { ActionModal } from "tideline-frontend";
import type { OwnedPosition, WrittenPosition } from "../../src/types";

// Modal overlay (scrim + centered panel). The transform wrapper keeps the
// fixed overlay in-card. Variants cover both kinds and each meaningful stage.
// The scrim is position:fixed; this in-flow spacer grows the provider's
// transform wrapper so the (tall) modal isn't clipped vertically.
const Frame = ({ children }: { children: React.ReactNode }) => (
  <div style={{ height: 860 }}>{children}</div>
);

const owned: OwnedPosition = {
  id: "0xowned_btc_1",
  side: "owned",
  asset: "BTC",
  strike: 96000,
  expiry: "2026-07-31",
  amount: 0.05,
  premiumPaid: 182.4,
  boughtFrom: "0x7a3f…9c21",
  boughtAt: "2026-06-12",
  rangeId: "0x4e1b…77d0",
  tradingAccountAmount: 0,
  lots: [{ amount: 0.05, cost: 182.4, source: "rfq", acquiredAtMs: 1718150400000 }],
  spot: 103250,
  dte: 41,
  itm: true,
  moneyness: 7.55,
  intrinsicNow: 362.5,
  pnl: 180.1,
  realizedPnl: 0,
  totalPnl: 180.1,
  unpricedExerciseAmount: 0,
  status: "exercisable",
};

const written: WrittenPosition = {
  id: "0xwritten_sui_1",
  side: "written",
  asset: "SUI",
  strike: 4.0,
  expiry: "2026-06-13",
  amount: 2000,
  premiumReceived: 168.0,
  soldTo: "0x3f90…ab7c",
  soldAt: "2026-05-30",
  rangeStart: 8.4,
  rangeEnd: 8.6,
  cursorAtSale: 8.35,
  cursorAtExpiry: 8.53,
  spot: 4.12,
  dte: -7,
  exercisedQty: 1300,
  totalQty: 2000,
  exercisedPct: 65,
  cursor: 8.53,
  status: "claimable",
};

const spots = { BTC: 103250, SUI: 4.12, USDC: 1 };

// Exercise review: full breakdown list (strike paid, asset received, intrinsic,
// net P/L) with cancel/confirm actions.
export const ExerciseReview = () => (
  <Frame><ActionModal
    modal={{ kind: "exercise", stage: "review", position: owned, qty: 0.05 }}
    spots={spots}
    onSubmit={() => {}}
    onClose={() => {}}
  /></Frame>
);

// Exercise success state: check glyph + paid/received/P-L recap + Done.
export const ExerciseConfirmed = () => (
  <Frame><ActionModal
    modal={{ kind: "exercise", stage: "confirmed", position: owned, qty: 0.05 }}
    spots={spots}
    onSubmit={() => {}}
    onClose={() => {}}
  /></Frame>
);

// Claim review: writer settlement breakdown (USDC from exercise, asset returned).
export const ClaimReview = () => (
  <Frame><ActionModal
    modal={{ kind: "claim", stage: "review", position: written }}
    spots={spots}
    onSubmit={() => {}}
    onClose={() => {}}
  /></Frame>
);

// In-flight signing state: spinner + waiting-for-wallet copy.
export const Signing = () => (
  <Frame><ActionModal
    modal={{ kind: "exercise", stage: "signing", position: owned, qty: 0.05 }}
    spots={spots}
    onSubmit={() => {}}
    onClose={() => {}}
  /></Frame>
);
