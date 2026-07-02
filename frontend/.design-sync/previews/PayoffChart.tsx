import { PayoffChart } from "tideline-frontend";

// Long-call payoff at expiry. pnl(S) = max(0, S − strike)·qty − totalCost.
// Pure props, no fetching. breakEven = strike + avgPrice (per-unit cost).

// 0.5 TBTC long call, strike 96000, premium ~340 USDC/BTC → totalCost 170.
export const InTheMoney = () => (
  <div style={{ width: 520 }}>
    <PayoffChart strike={96000} qty={0.5} totalCost={170} breakEven={96340} spot={101200} />
  </div>
);

// Spot sitting below strike — flat loss leg, dot on the floor.
export const OutOfTheMoney = () => (
  <div style={{ width: 520 }}>
    <PayoffChart strike={96000} qty={0.5} totalCost={170} breakEven={96340} spot={91500} />
  </div>
);

// Larger position: 2 TBTC, deeper premium, spot right at break-even.
export const AtBreakEven = () => (
  <div style={{ width: 520 }}>
    <PayoffChart strike={100000} qty={2} totalCost={920} breakEven={100460} spot={100460} />
  </div>
);
