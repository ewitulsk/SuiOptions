import { formatPrice } from "../format";
import type { Strike, View } from "../types";

// Vertical bucket list (SO-170): replaces the horizontal StrikeTiles carousel
// on the Buy screen's left rail. Denser rows instead of tiles — strike on the
// left, premium on the right, tier accent down the edge.

function formatStrike(s: number): string {
  return `$${formatPrice(s, { grouping: true })}`;
}

// Same tier ramp as StrikeTiles: writer hot→cool, trader inverted, spread
// evenly across the series so the full range is always used.
const AQUA_TIERS = ["--aqua-t0", "--aqua-t1", "--aqua-t2", "--aqua-t3", "--aqua-t4", "--aqua-t5"];

function tierVarFor(i: number, total: number, view: View): string {
  const pos = total <= 1 ? 0 : i / (total - 1);
  const ramp = view === "writer" ? pos : 1 - pos;
  const idx = Math.min(AQUA_TIERS.length - 1, Math.round(ramp * (AQUA_TIERS.length - 1)));
  return `var(${AQUA_TIERS[idx]})`;
}

type Props = {
  strikes: Strike[];
  selectedIdx: number;
  onSelect: (i: number) => void;
  view: View;
};

export function BucketList({ strikes, selectedIdx, onSelect, view }: Props) {
  return (
    <div className="bucketlist">
      <div className="bucketlist__label">strikes</div>
      {strikes.map((s, i) => {
        const tier = tierVarFor(i, strikes.length, view);
        return (
          <button
            // Two buckets can collide on display strike (different strike_scale
            // but same math); pair the strike with the index to keep React's
            // reconciler happy while rendering both rows.
            key={`${i}-${s.strike}`}
            className={"bucketrow" + (selectedIdx === i ? " is-selected" : "")}
            style={{ ["--tier-ink" as string]: tier } as React.CSSProperties}
            onClick={() => onSelect(i)}
          >
            <span className="bucketrow__strike">{formatStrike(s.strike)}</span>
            <span className={"bucketrow__premium" + (view === "trader" ? " bucketrow__premium--neg" : "")}>
              {view === "writer" ? "+" : "−"}
              {s.premiumDisplay}
            </span>
          </button>
        );
      })}
    </div>
  );
}
