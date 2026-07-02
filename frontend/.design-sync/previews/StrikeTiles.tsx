import { StrikeTiles } from "tideline-frontend";

// One bucket strike per tile, premium under the price. The tier ink ramps
// hot→cool across the series (writer view); trader view inverts and shows the
// premium as a cost (−).
const strikes = [
  { strike: 88000, perUnit: 0.0072, premium: 684, premiumDisplay: "684.00" },
  { strike: 92000, perUnit: 0.0051, premium: 485, premiumDisplay: "485.00" },
  { strike: 96000, perUnit: 0.0034, premium: 323, premiumDisplay: "323.00" },
  { strike: 100000, perUnit: 0.0021, premium: 199, premiumDisplay: "199.00" },
  { strike: 104000, perUnit: 0.0012, premium: 114, premiumDisplay: "114.00" },
  { strike: 108000, perUnit: 0.0006, premium: 57, premiumDisplay: "57.00" },
];

export const WriterView = () => (
  <StrikeTiles strikes={strikes} selectedIdx={2} onSelect={() => {}} view="writer" />
);

export const TraderView = () => (
  <StrikeTiles strikes={strikes} selectedIdx={3} onSelect={() => {}} view="trader" />
);
