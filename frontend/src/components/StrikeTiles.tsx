import type { Strike, View } from "../types";

// Format a strike for the tile. Large strikes get `$85k` shorthand;
// small strikes (e.g. seeded test buckets at $0.13) get full precision.
function formatStrike(s: number): string {
  if (s >= 1000) return `$${(s / 1000).toFixed(0)}k`;
  if (s >= 1) return `$${s.toLocaleString(undefined, { maximumFractionDigits: 2 })}`;
  return `$${s.toLocaleString(undefined, { maximumFractionDigits: 4 })}`;
}

// Tier ramp — writer view goes hot→cool (deep ITM = hot premium); trader inverts.
// Spread evenly across however many strikes are in the series so the ramp
// always uses the full range, regardless of bucket count.
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

export function StrikeTiles({ strikes, selectedIdx, onSelect, view }: Props) {
  return (
    <div className="tiles">
      {strikes.map((s, i) => {
        const tier = tierVarFor(i, strikes.length, view);
        return (
          <button
            // Two buckets in a series can scale to the same display
            // strike (e.g. one at strike_scale=0 and another at
            // strike_scale=2 whose math collides). They're distinct
            // bucket_ids so we want both tiles to render; pair the
            // strike with the index to keep React's reconciler happy.
            key={`${i}-${s.strike}`}
            className={"tile" + (selectedIdx === i ? " is-selected" : "")}
            style={{ ["--tier-ink" as string]: tier } as React.CSSProperties}
            onClick={() => onSelect(i)}
          >
            <span className="tile__strike">{formatStrike(s.strike)}</span>
            <span className={"tile__premium" + (view === "trader" ? " tile__premium--neg" : "")}>
              {view === "writer" ? "+" : "−"}
              {s.premiumDisplay}
            </span>
          </button>
        );
      })}
    </div>
  );
}
