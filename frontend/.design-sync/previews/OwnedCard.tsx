import { OwnedCard } from "tideline-frontend";
import type { OwnedPosition } from "../../src/types";

// A fully-decorated owned (long call) position. Built across the `status` enum
// so the footer CTA + moneyness bar render their distinct states.

const base: OwnedPosition = {
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
  lots: [
    { amount: 0.03, cost: 109.4, source: "rfq", acquiredAtMs: 1718150400000 },
    { amount: 0.02, cost: 73.0, source: "deepbook", acquiredAtMs: 1718323200000 },
  ],
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

// Spot above strike, in-the-money, exercise CTA live. Has a DeepBook
// trading-account slice + realized PnL row to exercise the dense layout.
export const Exercisable = () => (
  <OwnedCard
    p={{
      ...base,
      tradingAccountAmount: 0.02,
      realizedPnl: 44.6,
      totalPnl: 224.7,
    }}
    onExercise={() => {}}
    onWithdraw={() => {}}
  />
);

// SUI call sitting out of the money — hold + "exercise anyway" ghost/secondary CTAs.
export const ActiveOtm = () => (
  <OwnedCard
    p={{
      id: "0xowned_sui_2",
      side: "owned",
      asset: "SUI",
      strike: 4.5,
      expiry: "2026-07-15",
      amount: 1200,
      premiumPaid: 96.0,
      boughtFrom: "0x2c8a…44f1",
      boughtAt: "2026-06-09",
      rangeId: "0x91dd…0a3c",
      tradingAccountAmount: 0,
      lots: [{ amount: 1200, cost: 96.0, source: "rfq", acquiredAtMs: 1717891200000 }],
      spot: 4.12,
      dte: 25,
      itm: false,
      moneyness: -8.44,
      intrinsicNow: 0,
      pnl: -96.0,
      realizedPnl: 0,
      totalPnl: -96.0,
      unpricedExerciseAmount: 0,
      status: "active_otm",
    }}
    onExercise={() => {}}
    onWithdraw={() => {}}
  />
);

// Expired while in the money — exercise window closed, disabled ghost CTA.
export const ExpiredItm = () => (
  <OwnedCard
    p={{
      ...base,
      id: "0xowned_btc_3",
      expiry: "2026-06-13",
      dte: -7,
      premiumPaid: 211.0,
      pnl: 151.5,
      intrinsicNow: 362.5,
      realizedPnl: 0,
      totalPnl: 151.5,
      lots: [{ amount: 0.05, cost: 211.0, source: "rfq", acquiredAtMs: 1717459200000 }],
      status: "expired_itm",
    }}
    onExercise={() => {}}
    onWithdraw={() => {}}
  />
);
