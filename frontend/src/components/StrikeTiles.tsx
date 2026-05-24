import type { Strike, View } from "../types";

// Format a strike for the tile. Large strikes get `$85k` shorthand;
// small strikes (e.g. seeded test buckets at $0.13) get full precision.
function formatStrike(s: number): string {
  if (s >= 1000) return `$${(s / 1000).toFixed(0)}k`;
  if (s >= 1) return `$${s.toLocaleString(undefined, { maximumFractionDigits: 2 })}`;
  return `$${s.toLocaleString(undefined, { maximumFractionDigits: 4 })}`;
}

// Tier ramp — writer view goes hot→cool (deep ITM = hot premium); trader inverts.
const AQUA_TIERS = ["--aqua-t0", "--aqua-t1", "--aqua-t2", "--aqua-t3", "--aqua-t4", "--aqua-t5"];
const AQUA_TIER_LABELS = ["deep ITM", "near ITM", "at money", "near OTM", "far OTM", "deep OTM"];

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
        const tIdx = view === "writer" ? i : strikes.length - 1 - i;
        const tier = `var(${AQUA_TIERS[tIdx]})`;
        const label = AQUA_TIER_LABELS[i];
        return (
          <button
            key={s.strike}
            className={"tile" + (selectedIdx === i ? " is-selected" : "")}
            style={{ ["--tier-ink" as string]: tier } as React.CSSProperties}
            onClick={() => onSelect(i)}
          >
            <span className="tile__tier">{label}</span>
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
