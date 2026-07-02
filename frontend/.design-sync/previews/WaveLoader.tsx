import { WaveLoader } from "tideline-frontend";

// Cresting-wave line shown in place of the hero premium while a firm RFQ
// quote is in flight. Pure presentational SVG; spans its container width.

export const Default = () => (
  <div style={{ width: 240 }}>
    <WaveLoader />
  </div>
);

// In context: sized like the hero premium slot it replaces while quoting.
export const HeroSlot = () => (
  <div style={{ width: 320, fontSize: 28 }}>
    <WaveLoader />
  </div>
);
