// Long-call payoff-at-expiry diagram (SO-199). Pure frontend — every value is
// derived from props, no data fetching.
//
//   pnl(S) = max(0, S − strike) · qty − totalCost
//
// Flat loss segment at −totalCost up to the strike, then a linear ramp; a zero
// line, a dashed break-even marker (strike + avgPrice), and a dot at the
// current spot. Profit area shades green, loss area red, matching the
// competitor screen. Colors come from the aqua theme vars (see aqua.css).

import { formatPrice } from "../format";

type Props = {
  strike: number;
  qty: number;
  totalCost: number;
  breakEven: number;
  spot: number;
};

const W = 520;
const H = 200;
const PAD = { l: 12, r: 12, t: 16, b: 22 };

export function PayoffChart({ strike, qty, totalCost, breakEven, spot }: Props) {
  const plotW = W - PAD.l - PAD.r;
  const plotH = H - PAD.t - PAD.b;

  // Domain centered on the strike; widen if spot sits outside so its marker
  // stays on-canvas.
  const xMin = Math.min(0.5 * strike, spot * 0.95);
  const xMax = Math.max(1.5 * strike, spot * 1.05);

  const pnl = (s: number) => Math.max(0, s - strike) * qty - totalCost;

  const yLo = -totalCost;
  const yHi = pnl(xMax);
  const yPad = (yHi - yLo) * 0.12 || 1;
  const yMin = yLo - yPad;
  const yMax = yHi + yPad;

  const xPx = (s: number) => PAD.l + ((s - xMin) / (xMax - xMin)) * plotW;
  const yPx = (v: number) => PAD.t + ((yMax - v) / (yMax - yMin)) * plotH;

  const zeroY = yPx(0);
  const beX = xPx(Math.min(Math.max(breakEven, xMin), xMax));
  const spotClamped = Math.min(Math.max(spot, xMin), xMax);

  // Payoff polyline vertices: flat to the strike, then the ramp.
  const p0 = { x: xPx(xMin), y: yPx(pnl(xMin)) };
  const pK = { x: xPx(strike), y: yPx(pnl(strike)) };
  const p1 = { x: xPx(xMax), y: yPx(pnl(xMax)) };
  const linePts = `${p0.x},${p0.y} ${pK.x},${pK.y} ${p1.x},${p1.y}`;

  // Loss area (below zero, left of break-even) and profit area (above zero).
  const lossPoly = `${p0.x},${zeroY} ${p0.x},${p0.y} ${pK.x},${pK.y} ${beX},${zeroY}`;
  const profitPoly = `${beX},${zeroY} ${p1.x},${p1.y} ${p1.x},${zeroY}`;

  return (
    <div className="payoff">
      <div className="panel__head">
        payoff at expiry
      </div>
      <svg
        viewBox={`0 0 ${W} ${H}`}
        preserveAspectRatio="none"
        style={{ width: "100%", height: H, display: "block" }}
      >
        <polygon points={lossPoly} fill="var(--aqua-down)" opacity={0.16} />
        <polygon points={profitPoly} fill="var(--aqua-up)" opacity={0.16} />

        {/* zero line */}
        <line
          x1={PAD.l}
          y1={zeroY}
          x2={W - PAD.r}
          y2={zeroY}
          stroke="var(--aqua-ink-3)"
          strokeWidth={1}
          opacity={0.5}
        />
        {/* break-even marker */}
        <line
          x1={beX}
          y1={PAD.t}
          x2={beX}
          y2={H - PAD.b}
          stroke="var(--aqua-ink-2)"
          strokeWidth={1}
          strokeDasharray="4 4"
        />

        {/* payoff curve */}
        <polyline
          points={linePts}
          fill="none"
          stroke="var(--aqua-accent)"
          strokeWidth={2}
          strokeLinejoin="round"
        />

        {/* current-spot dot on the curve */}
        <circle
          cx={xPx(spotClamped)}
          cy={yPx(pnl(spotClamped))}
          r={4}
          fill="var(--aqua-accent)"
        />

        <text x={beX + 4} y={H - PAD.b + 16} fill="var(--aqua-ink-3)" fontSize={10}>
          BE ${formatPrice(breakEven)}
        </text>
      </svg>
    </div>
  );
}
