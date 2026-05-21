type WriterProps = { premium: number; amount: number; strike: number };

export function WriterPanels({ premium, amount, strike }: WriterProps) {
  return (
    <div className="panels">
      <div className="panel">
        <div className="panel__head">
          <span className="panel__head-dot"></span>now · premium
        </div>
        <div className="panel__hero">
          {premium.toFixed(2)}
          <span className="unit">USDC</span>
        </div>
        <div className="panel__sub">
          Paid upfront. Yours to keep regardless of exercise.
        </div>
      </div>
      <div className="panel">
        <div className="panel__head">
          <span className="panel__head-dot"></span>on expiry · jun 26
        </div>
        <div
          className="panel__split"
          style={{ margin: 0, padding: 0, background: "transparent", border: "none" }}
        >
          <div className="panel__split-cell">
            <div className="panel__split-label">if BTC ≥ ${strike.toLocaleString("en-US")}</div>
            <div className="panel__split-val">
              {(amount * strike).toLocaleString("en-US", {
                minimumFractionDigits: 2,
                maximumFractionDigits: 2,
              })}
              <span style={{ fontSize: 11, color: "var(--aqua-ink-3)", marginLeft: 4 }}>USDC</span>
            </div>
            <div className="panel__split-sub">
              your {amount.toFixed(4)} BTC is sold at strike
            </div>
          </div>
          <div className="panel__split-cell">
            <div className="panel__split-label">if BTC &lt; ${strike.toLocaleString("en-US")}</div>
            <div className="panel__split-val">
              {amount.toFixed(4)}
              <span style={{ fontSize: 11, color: "var(--aqua-ink-3)", marginLeft: 4 }}>BTC</span>
            </div>
            <div className="panel__split-sub">your collateral returns to you</div>
          </div>
        </div>
      </div>
    </div>
  );
}

type TraderProps = { premium: number; amount: number; strike: number; spot: number };

export function TraderPanels({ premium, amount, strike, spot }: TraderProps) {
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
            −{premium.toFixed(2)}
            <span className="unit">USDC</span>
          </div>
          <div className="panel__sub">
            For the right to buy <b>{amount.toFixed(4)} BTC</b> at{" "}
            <b>${strike.toLocaleString("en-US")}</b> any time before Jun 26th.
          </div>
        </div>
        <div className="panel">
          <div className="panel__head">
            <span className="panel__head-dot"></span>exercise · anytime before jun 26
          </div>
          <div
            className="panel__split"
            style={{ margin: 0, padding: 0, background: "transparent", border: "none" }}
          >
            <div className="panel__split-cell">
              <div className="panel__split-label">pay</div>
              <div className="panel__split-val">
                {(strike * amount).toLocaleString("en-US", { maximumFractionDigits: 0 })}
                <span style={{ fontSize: 11, color: "var(--aqua-ink-3)", marginLeft: 4 }}>USDC</span>
              </div>
            </div>
            <div className="panel__split-cell">
              <div className="panel__split-label">receive</div>
              <div className="panel__split-val panel__split-val--pos">
                {amount.toFixed(4)}
                <span style={{ fontSize: 11, color: "var(--aqua-ink-3)", marginLeft: 4 }}>BTC</span>
              </div>
            </div>
          </div>
        </div>
      </div>
      <div className="exercise">
        <div className="exercise__cell">
          <div className="exercise__label">breakeven</div>
          <div className="exercise__value">${Math.round(breakeven).toLocaleString("en-US")}</div>
          <div className="exercise__sub">spot needs to close above this</div>
        </div>
        <div className="exercise__cell">
          <div className="exercise__label">max loss</div>
          <div className="exercise__value exercise__value--neg">−{premium.toFixed(2)}</div>
          <div className="exercise__sub">if BTC ≤ ${strike.toLocaleString("en-US")} at expiry</div>
        </div>
        <div className="exercise__cell">
          <div className="exercise__label">at +20% spot</div>
          <div className="exercise__value exercise__value--pos">+{upside.toFixed(2)}</div>
          <div className="exercise__sub">
            P/L if BTC reaches ${Math.round(spot * 1.2).toLocaleString("en-US")}
          </div>
        </div>
      </div>
    </>
  );
}
