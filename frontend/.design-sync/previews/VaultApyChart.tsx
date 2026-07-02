import { VaultApyChart } from "tideline-frontend";

// APY-over-time: solid green realized line (finalized rounds) + dashed accent
// projected line anchored to the last realized point, with a confidence band
// and the per-round assignment-risk footer. apy is a fraction (0.18 = 18%).

const DAY = 86_400_000;
const t0 = Date.parse("2026-04-01T00:00:00Z");

const realized = [
  { t_ms: t0 + 0 * 7 * DAY, apy: 0.142 },
  { t_ms: t0 + 1 * 7 * DAY, apy: 0.168 },
  { t_ms: t0 + 2 * 7 * DAY, apy: 0.155 },
  { t_ms: t0 + 3 * 7 * DAY, apy: 0.191 },
  { t_ms: t0 + 4 * 7 * DAY, apy: 0.173 },
  { t_ms: t0 + 5 * 7 * DAY, apy: 0.205 },
  { t_ms: t0 + 6 * 7 * DAY, apy: 0.188 },
];

const predicted = [
  {
    t_ms: t0 + 7 * 7 * DAY,
    apy: 0.196,
    apy_low: 0.12,
    apy_high: 0.27,
    kind: "current",
    confidence: 0.7,
    assignment_prob: 0.31,
    downside_round_yield: -0.018,
  },
];

export const RealizedAndProjected = () => (
  <div style={{ width: 560 }}>
    <VaultApyChart realized={realized} predicted={predicted} loading={false} />
  </div>
);

// Several finalized rounds, no forecast deployed yet → just the realized curve.
export const RealizedOnly = () => (
  <div style={{ width: 560 }}>
    <VaultApyChart realized={realized} predicted={[]} loading={false} />
  </div>
);

// Fresh vault, no rounds settled — the "coming soon" empty state.
export const Empty = () => (
  <div style={{ width: 560 }}>
    <VaultApyChart realized={[]} predicted={[]} loading={false} />
  </div>
);
