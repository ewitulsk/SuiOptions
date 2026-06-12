import { formatPremiumFull, formatPrice } from "../format";
import { WaveLoader } from "./WaveLoader";

// Token quantities keep up to 4 decimals; USD prices go through formatPrice.
const fmt = (n: number) =>
  n.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 4 });
const usd = (n: number) => formatPrice(n, { grouping: true });
// Premiums show the exact amount paid/earned, never rounded to the cent.
const premiumFull = (n: number) => formatPremiumFull(n, { grouping: true });

type WriterProps = {
  premium: number;
  premiumLoading: boolean;
  amount: number;
  strike: number;
  assetSymbol: string | null;
  expiryLabel: string;
};

export function WriterPanels({ premium, premiumLoading, amount, strike, assetSymbol, expiryLabel }: WriterProps) {
  const asset = assetSymbol ?? "—";
  return (
    <div className="panels">
      <div className="panel">
        <div className="panel__head">
          <span className="panel__head-dot"></span>now · premium
        </div>
        <div className="panel__hero">
          {premiumLoading ? <WaveLoader /> : premiumFull(premium)}
          <span className="unit">USDC</span>
        </div>
        <div className="panel__sub">
          Paid upfront. Yours to keep regardless of exercise.
        </div>
      </div>
      <div className="panel">
        <div className="panel__head">
          <span className="panel__head-dot"></span>on expiry · {expiryLabel}
        </div>
        <div
          className="panel__split"
          style={{ margin: 0, padding: 0, background: "transparent", border: "none" }}
        >
          <div className="panel__split-cell">
            <div className="panel__split-label">if {asset} ≥ ${usd(strike)}</div>
            <div className="panel__split-val">
              {usd(amount * strike)}
              <span style={{ fontSize: 11, color: "var(--aqua-ink-3)", marginLeft: 4 }}>USDC</span>
            </div>
            <div className="panel__split-sub">
              your {fmt(amount)} {asset} is sold at strike
            </div>
          </div>
          <div className="panel__split-cell">
            <div className="panel__split-label">if {asset} &lt; ${usd(strike)}</div>
            <div className="panel__split-val">
              {fmt(amount)}
              <span style={{ fontSize: 11, color: "var(--aqua-ink-3)", marginLeft: 4 }}>{asset}</span>
            </div>
            <div className="panel__split-sub">your collateral returns to you</div>
          </div>
        </div>
      </div>
    </div>
  );
}

type TraderProps = {
  premium: number;
  premiumLoading: boolean;
  amount: number;
  strike: number;
  spot: number;
  assetSymbol: string | null;
  expiryLabel: string;
};

export function TraderPanels({ premium, premiumLoading, amount, strike, spot, assetSymbol, expiryLabel }: TraderProps) {
  const asset = assetSymbol ?? "—";
  const breakeven = strike + premium / amount;
  const upside = Math.max(0, (spot * 1.2 - strike) * amount - premium);
  return (
    <>
      <div className="panels">
        <div className="panel">
          <div className="panel__head">
            <span className="panel__head-dot"></span>you pay · now
          </div>
          <div className="panel__hero">
            {premiumLoading ? <WaveLoader /> : <>−{premiumFull(premium)}</>}
            <span className="unit">USDC</span>
          </div>
          <div className="panel__sub">
            For the right to buy <b>{fmt(amount)} {asset}</b> at{" "}
            <b>${usd(strike)}</b> any time before {expiryLabel}.
          </div>
        </div>
        <div className="panel">
          <div className="panel__head">
            <span className="panel__head-dot"></span>exercise · anytime before {expiryLabel}
          </div>
          <div
            className="panel__split"
            style={{ margin: 0, padding: 0, background: "transparent", border: "none" }}
          >
            <div className="panel__split-cell">
              <div className="panel__split-label">pay</div>
              <div className="panel__split-val">
                {usd(strike * amount)}
                <span style={{ fontSize: 11, color: "var(--aqua-ink-3)", marginLeft: 4 }}>USDC</span>
              </div>
            </div>
            <div className="panel__split-cell">
              <div className="panel__split-label">receive</div>
              <div className="panel__split-val panel__split-val--pos">
                {fmt(amount)}
                <span style={{ fontSize: 11, color: "var(--aqua-ink-3)", marginLeft: 4 }}>{asset}</span>
              </div>
            </div>
          </div>
        </div>
      </div>
      <div className="exercise">
        <div className="exercise__cell">
          <div className="exercise__label">breakeven</div>
          <div className="exercise__value">${usd(breakeven)}</div>
          <div className="exercise__sub">spot needs to close above this</div>
        </div>
        <div className="exercise__cell">
          <div className="exercise__label">max loss</div>
          <div className="exercise__value exercise__value--neg">−{premiumFull(premium)}</div>
          <div className="exercise__sub">if {asset} ≤ ${usd(strike)} at expiry</div>
        </div>
        <div className="exercise__cell">
          <div className="exercise__label">at +20% spot</div>
          <div className="exercise__value exercise__value--pos">+{usd(upside)}</div>
          <div className="exercise__sub">
            P/L if {asset} reaches ${usd(spot * 1.2)}
          </div>
        </div>
      </div>
    </>
  );
}
