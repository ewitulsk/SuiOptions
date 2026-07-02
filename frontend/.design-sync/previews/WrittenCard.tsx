import { WrittenCard } from "tideline-frontend";
import type { WrittenPosition } from "../../src/types";

// A fully-decorated written (short call) position. Variants span the `status`
// enum so the range bar fill + footer CTA render their distinct states.

const base: WrittenPosition = {
  id: "0xwritten_btc_1",
  side: "written",
  asset: "BTC",
  strike: 100000,
  expiry: "2026-07-31",
  amount: 0.1,
  premiumReceived: 314.2,
  soldTo: "0x6b2e…d18a",
  soldAt: "2026-06-10",
  rangeStart: 1.2,
  rangeEnd: 1.3,
  cursorAtSale: 1.18,
  cursorAtExpiry: 1.3,
  spot: 103250,
  dte: 41,
  exercisedQty: 0,
  totalQty: 0.1,
  exercisedPct: 0,
  cursor: 1.24,
  status: "active",
};

// Live, not yet exercised — ghost CTA + "expires in Nd" foot note, empty range fill.
export const Active = () => (
  <WrittenCard
    p={{ ...base, exercisedQty: 0, exercisedPct: 0, cursor: 1.2 }}
    onClaim={() => {}}
  />
);

// Bucket cursor has crossed partway into the writer's range.
export const PartiallyExercised = () => (
  <WrittenCard
    p={{
      ...base,
      id: "0xwritten_btc_2",
      exercisedQty: 0.045,
      totalQty: 0.1,
      exercisedPct: 45,
      cursor: 1.245,
      status: "partially_exercised",
    }}
    onClaim={() => {}}
  />
);

// Expired, settlement claimable — primary CTA shows USD-equivalent payout.
export const Claimable = () => (
  <WrittenCard
    p={{
      id: "0xwritten_sui_3",
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
    }}
    onClaim={() => {}}
  />
);
