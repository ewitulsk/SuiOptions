import { usePythPrice } from "../api/usePythPrice";

type Props = { symbol: string | null | undefined; capPct: number };

export function BucketBar({ symbol, capPct }: Props) {
  const r = 12;
  const c = 2 * Math.PI * r;
  const live = usePythPrice(symbol);
  const priceLabel = live
    ? `$${live.price.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`
    : "—";
  return (
    <div className="bbar">
      <div className="bbar__sel">
        <span className="bbar__sel-icon">₿</span>
        <span className="bbar__sel-label">BTC</span>
        <span className="bbar__caret">▾</span>
      </div>
      <div className="bbar__sel">
        <span className="bbar__sel-label">Covered call</span>
        <span className="bbar__caret">▾</span>
      </div>
      <div className="bbar__sel">
        <span className="bbar__sel-label bbar__sel-label--mono">JUN_26</span>
        <span className="bbar__caret">▾</span>
      </div>
      <div className="bbar__sel" style={{ borderRight: "none" }}>
        <span className="bbar__sel-label--mono" style={{ fontSize: 10, color: "var(--aqua-ink-3)" }}>
          settled in
        </span>
        <span className="bbar__sel-label">USDC</span>
      </div>
      <div className="bbar__spacer"></div>
      <div className="bbar__price">
        <div>
          <div className="bbar__price-val">{priceLabel}</div>
          <div className="bbar__price-tick">{live ? "spot live · pyth" : "connecting…"}</div>
        </div>
        <div className="bbar__cap">
          <div className="bbar__cap-ring">
            <svg width="32" height="32" viewBox="0 0 32 32">
              <circle cx="16" cy="16" r={r} fill="none" stroke="rgba(11,37,69,0.10)" strokeWidth="3" />
              <circle
                cx="16"
                cy="16"
                r={r}
                fill="none"
                stroke="var(--aqua-sui)"
                strokeWidth="3"
                strokeDasharray={`${(c * capPct) / 100} ${c}`}
                strokeLinecap="round"
              />
            </svg>
          </div>
          <div className="bbar__cap-text">
            <b>{capPct}%</b>
            <br />
            of cap
          </div>
        </div>
      </div>
    </div>
  );
}
