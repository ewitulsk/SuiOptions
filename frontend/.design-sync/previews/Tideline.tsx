import { Tideline } from "tideline-frontend";

// The writer's "place in the queue" visual: an animated wave bar showing the
// exercised zone, the queue ahead, and the writer's own position highlighted.

export const Queued = () => (
  <Tideline
    bucket={{ cursor: 2.4, queued: 3.1, cap: 12.5 }}
    amount={0.5}
    assetSymbol="TBTC"
  />
);

export const FrontOfQueue = () => (
  <Tideline
    bucket={{ cursor: 0.8, queued: 0.2, cap: 12.5 }}
    amount={1.0}
    assetSymbol="TBTC"
  />
);

export const NearlyExercised = () => (
  <Tideline
    bucket={{ cursor: 9.6, queued: 1.4, cap: 12.5 }}
    amount={0.8}
    assetSymbol="SUI"
  />
);
