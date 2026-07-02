import { TraderPanels } from "tideline-frontend";

// The trader-side economics panels: premium paid now, the exercise pay/receive
// split, and the breakeven · max-loss · upside scenario row.

export const Call = () => (
  <TraderPanels
    premium={368}
    premiumLoading={false}
    amount={0.1}
    strike={96000}
    spot={96850}
    assetSymbol="TBTC"
    expiryLabel="Jun 26"
  />
);

export const Loading = () => (
  <TraderPanels
    premium={0}
    premiumLoading={true}
    amount={0.1}
    strike={96000}
    spot={96850}
    assetSymbol="TBTC"
    expiryLabel="Jun 26"
  />
);

export const SuiCall = () => (
  <TraderPanels
    premium={42.5}
    premiumLoading={false}
    amount={250}
    strike={4.2}
    spot={3.95}
    assetSymbol="SUI"
    expiryLabel="Jul 31"
  />
);
