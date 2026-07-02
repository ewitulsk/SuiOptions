import { WriterPanels } from "tideline-frontend";

// The writer-side economics panels: premium earned upfront and the on-expiry
// split (asset sold at strike if ITM vs. collateral returned if OTM).

export const CoveredCall = () => (
  <WriterPanels
    premium={368}
    premiumLoading={false}
    amount={0.1}
    strike={96000}
    assetSymbol="TBTC"
    expiryLabel="Jun 26"
  />
);

export const Loading = () => (
  <WriterPanels
    premium={0}
    premiumLoading={true}
    amount={0.1}
    strike={96000}
    assetSymbol="TBTC"
    expiryLabel="Jun 26"
  />
);

export const SuiCoveredCall = () => (
  <WriterPanels
    premium={42.5}
    premiumLoading={false}
    amount={250}
    strike={4.2}
    assetSymbol="SUI"
    expiryLabel="Jul 31"
  />
);
