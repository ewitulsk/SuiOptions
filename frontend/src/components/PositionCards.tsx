import type { OwnedPosition, WrittenPosition } from "../types";
import { formatPrice } from "../format";
import { TokenLogo } from "./TokenLogo";

function AssetGlyph({ asset }: { asset: string }) {
  const fallback =
    asset === "BTC" ? (
      <span className="asset-glyph asset-glyph--btc">₿</span>
    ) : asset === "SUI" ? (
      <span className="asset-glyph asset-glyph--sui">≈</span>
    ) : (
      <span className="asset-glyph">{asset[0]}</span>
    );
  return <TokenLogo symbol={asset} className="asset-glyph" fallback={fallback} />;
}

function StatusPill({ status }: { status: string }) {
  const map: Record<string, { label: string; cls: string }> = {
    exercisable:         { label: "exercisable",         cls: "is-pos"  },
    active_otm:          { label: "out of money",        cls: "is-mute" },
    expired_itm:         { label: "expired · ITM",       cls: "is-warn" },
    expired_otm:         { label: "expired · OTM",       cls: "is-mute" },
    claimable:           { label: "claimable",           cls: "is-pos"  },
    active:              { label: "active",              cls: "is-mute" },
    partially_exercised: { label: "partial · exercised", cls: "is-info" },
    fully_exercised:     { label: "fully exercised",     cls: "is-info" },
  };
  const m = map[status] ?? { label: status, cls: "" };
  return <span className={"status-pill " + m.cls}>{m.label}</span>;
}

const fmtAmt = (asset: string, n: number) => (asset === "BTC" ? n.toFixed(4) : n.toFixed(0));
const LOT_SOURCE_LABEL: Record<string, string> = {
  rfq: "market maker",
  deepbook: "DeepBook",
  transfer: "transferred in",
};
const fmtStrike = (asset: string, n: number) =>
  asset === "BTC" ? `$${(n / 1000).toFixed(0)}k` : `$${formatPrice(n)}`;
const fmtSpot = (asset: string, n: number) =>
  asset === "BTC"
    ? `$${n.toLocaleString("en-US", { maximumFractionDigits: 0 })}`
    : `$${formatPrice(n)}`;

export function OwnedCard({
  p,
  onExercise,
  onWithdraw,
}: {
  p: OwnedPosition;
  onExercise: (p: OwnedPosition) => void;
  onWithdraw: (p: OwnedPosition) => void;
}) {
  const isItm = p.itm;
  const isPut = p.optionType === "put";
  const inDeepbook = p.tradingAccountAmount > 0;
  return (
    <div className={"pos-card pos-card--owned " + (isItm ? "is-itm" : "is-otm")}>
      <div className="pos-card__head">
        <div className="pos-card__bucket">
          <AssetGlyph asset={p.asset} />
          <div className="pos-card__bucket-label">
            <div className="pos-card__bucket-main">
              {p.asset} <span className="dim">·</span> {isPut ? "put" : "call"} <span className="dim">·</span>{" "}
              {fmtStrike(p.asset, p.strike)}
            </div>
            <div className="pos-card__bucket-sub">
              expires{" "}
              <b>
                {new Date(p.expiry).toLocaleDateString("en-US", {
                  month: "short",
                  day: "numeric",
                })}
              </b>{" "}
              · {p.dte > 0 ? `in ${p.dte}d` : "today"} · token {p.rangeId}
            </div>
          </div>
        </div>
        <StatusPill status={p.status} />
      </div>

      <div className="pos-card__body">
        <div className="pos-card__metric">
          <div className="pos-card__metric-label">You own</div>
          <div className="pos-card__metric-val">
            {fmtAmt(p.asset, p.amount)}
            <span className="pos-card__metric-unit"> {p.asset} {isPut ? "put" : "call"}</span>
          </div>
          <div className="pos-card__metric-sub">
            cost basis {formatPrice(p.premiumPaid)} USDC
          </div>
          {p.lots.map((lot, i) => (
            <div className="pos-card__metric-sub dim" key={i}>
              {fmtAmt(p.asset, lot.amount)} ·{" "}
              {lot.source === "transfer"
                ? "transferred in · $0"
                : `${formatPrice(lot.cost)} USDC · ${LOT_SOURCE_LABEL[lot.source] ?? lot.source}`}
            </div>
          ))}
          <div className="pos-card__metric-sub">
            {isPut
              ? `deliver ${fmtAmt(p.asset, p.amount)} ${p.asset} · receive ${formatPrice(p.amount * p.strike, { grouping: true })} USDC`
              : `exercise cost ${formatPrice(p.amount * p.strike, { grouping: true })} USDC`}
          </div>
          {inDeepbook && (
            <div className="pos-card__metric-sub is-info">
              {fmtAmt(p.asset, p.tradingAccountAmount)} in trading account ·{" "}
              <button
                onClick={() => onWithdraw(p)}
                title="Withdraw this position token from DeepBook to your wallet"
                style={{
                  background: "transparent",
                  border: "none",
                  padding: 0,
                  font: "inherit",
                  color: "inherit",
                  textDecoration: "underline",
                  cursor: "pointer",
                }}
              >
                withdraw
              </button>
            </div>
          )}
        </div>
        <div className="pos-card__metric">
          <div className="pos-card__metric-label">Spot now</div>
          <div className="pos-card__metric-val">{fmtSpot(p.asset, p.spot)}</div>
          <div className={"pos-card__metric-sub " + (isItm ? "is-pos" : "is-neg")}>
            {isItm ? "+" : ""}
            {p.moneyness.toFixed(2)}% vs strike
          </div>
        </div>
        <div className="pos-card__metric">
          <div className="pos-card__metric-label">
            {isItm ? "If exercised now" : "If exercised today"}
          </div>
          <div className={"pos-card__metric-val " + (p.pnl >= 0 ? "is-pos" : "is-neg")}>
            {p.pnl >= 0 ? "+" : "−"}
            {formatPrice(Math.abs(p.pnl))}
            <span className="pos-card__metric-unit"> USDC</span>
          </div>
          <div className="pos-card__metric-sub">
            {isItm
              ? `intrinsic ${formatPrice(p.intrinsicNow)} − cost ${formatPrice(p.premiumPaid)}`
              : `OTM · would lose cost basis`}
          </div>
          {(p.realizedPnl !== 0 || p.unpricedExerciseAmount > 0) && (
            <div className="pos-card__metric-sub">
              realized {p.realizedPnl >= 0 ? "+" : "−"}
              {formatPrice(Math.abs(p.realizedPnl))} · total{" "}
              <span className={p.totalPnl >= 0 ? "is-pos" : "is-neg"}>
                {p.totalPnl >= 0 ? "+" : "−"}
                {formatPrice(Math.abs(p.totalPnl))}
              </span>{" "}
              USDC
              {p.unpricedExerciseAmount > 0
                ? ` · ${fmtAmt(p.asset, p.unpricedExerciseAmount)} exercised unpriced`
                : ""}
            </div>
          )}
        </div>
      </div>

      <div className="moneyness">
        <div className="moneyness__track">
          <div className="moneyness__strike" title={`strike ${fmtStrike(p.asset, p.strike)}`} />
          <div
            className="moneyness__spot"
            style={{
              left: `${Math.max(2, Math.min(98, 50 + Math.max(-30, Math.min(30, p.moneyness))))}%`,
            }}
          >
            <div className="moneyness__spot-label">spot · {fmtSpot(p.asset, p.spot)}</div>
          </div>
        </div>
        <div className="moneyness__legend">
          <span>OTM</span>
          <span className="moneyness__legend-mid">strike {fmtStrike(p.asset, p.strike)}</span>
          <span>ITM →</span>
        </div>
      </div>

      <div className="pos-card__foot">
        {p.status === "exercisable" && (
          <button
            className="pos-card__cta pos-card__cta--primary"
            onClick={() => onExercise(p)}
          >
            {isPut
              ? `Exercise → deliver ${fmtAmt(p.asset, p.amount)} ${p.asset}`
              : `Exercise → receive ${fmtAmt(p.asset, p.amount)} ${p.asset}`}
          </button>
        )}
        {p.status === "expired_itm" && (
          <button className="pos-card__cta pos-card__cta--ghost" disabled>
            Expired · exercise window closed
          </button>
        )}
        {p.status === "active_otm" && (
          <>
            <button className="pos-card__cta pos-card__cta--ghost" disabled>
              Hold · wait for {p.asset} {isPut ? "≤" : "≥"} {fmtStrike(p.asset, p.strike)}
            </button>
            <button
              className="pos-card__cta pos-card__cta--secondary"
              onClick={() => onExercise(p)}
            >
              {isPut
                ? `Exercise anyway → deliver ${fmtAmt(p.asset, p.amount)} ${p.asset}`
                : `Exercise anyway → receive ${fmtAmt(p.asset, p.amount)} ${p.asset}`}
            </button>
          </>
        )}
        {p.status === "expired_otm" && (
          <button className="pos-card__cta pos-card__cta--ghost" disabled>
            Expired worthless · no action
          </button>
        )}
      </div>
    </div>
  );
}

export function WrittenCard({
  p,
  onClaim,
}: {
  p: WrittenPosition;
  onClaim: (p: WrittenPosition) => void;
}) {
  const exercisedPct = p.exercisedPct;
  const isPut = p.optionType === "put";
  return (
    <div className="pos-card pos-card--written">
      <div className="pos-card__head">
        <div className="pos-card__bucket">
          <AssetGlyph asset={p.asset} />
          <div className="pos-card__bucket-label">
            <div className="pos-card__bucket-main">
              {p.asset} <span className="dim">·</span> {isPut ? "put" : "call"} <span className="dim">·</span>{" "}
              {fmtStrike(p.asset, p.strike)}
            </div>
            <div className="pos-card__bucket-sub">
              expires{" "}
              <b>
                {new Date(p.expiry).toLocaleDateString("en-US", {
                  month: "short",
                  day: "numeric",
                })}
              </b>{" "}
              · {p.dte > 0 ? `in ${p.dte}d` : `${Math.abs(p.dte)}d ago`} · sold to {p.soldTo}
            </div>
          </div>
        </div>
        <StatusPill status={p.status} />
      </div>

      <div className="pos-card__body">
        <div className="pos-card__metric">
          <div className="pos-card__metric-label">You wrote</div>
          <div className="pos-card__metric-val">
            {fmtAmt(p.asset, p.amount)}
            <span className="pos-card__metric-unit"> {p.asset}</span>
          </div>
          <div className="pos-card__metric-sub">collateral locked in bucket</div>
        </div>
        <div className="pos-card__metric">
          <div className="pos-card__metric-label">Premium kept</div>
          <div className="pos-card__metric-val is-pos">
            +{formatPrice(p.premiumReceived)}
            <span className="pos-card__metric-unit"> USDC</span>
          </div>
          <div className="pos-card__metric-sub">yours regardless</div>
        </div>
        <div className="pos-card__metric">
          <div className="pos-card__metric-label">Exercised against you</div>
          <div className="pos-card__metric-val">
            {exercisedPct.toFixed(0)}
            <span className="pos-card__metric-unit">% of range</span>
          </div>
          <div className="pos-card__metric-sub">
            {fmtAmt(p.asset, p.exercisedQty)} / {fmtAmt(p.asset, p.totalQty)} {p.asset}
          </div>
        </div>
      </div>

      <div className="rangebar">
        <div className="rangebar__head">
          <span className="rangebar__title">your range</span>
          <span className="rangebar__caption">
            [{p.rangeStart.toFixed(3)}, {p.rangeEnd.toFixed(3)})
          </span>
        </div>
        <div className="rangebar__track">
          <div className="rangebar__fill" style={{ width: `${exercisedPct}%` }}></div>
          <div className="rangebar__cursor" style={{ left: `${exercisedPct}%` }}></div>
        </div>
        <div className="rangebar__legend">
          <span>
            {fmtAmt(p.asset, p.exercisedQty)} {p.asset} exercised
          </span>
          <span>
            {fmtAmt(p.asset, p.totalQty - p.exercisedQty)} {p.asset} remaining
          </span>
        </div>
      </div>

      <div className="pos-card__foot">
        {p.status === "claimable" && (
          <button
            className="pos-card__cta pos-card__cta--primary"
            onClick={() => onClaim(p)}
          >
            Claim settlement → +
            {formatPrice(
              p.exercisedQty * p.strike + (p.totalQty - p.exercisedQty) * p.spot,
              { grouping: true },
            )}{" "}
            USD equivalent
          </button>
        )}
        {(p.status === "active" || p.status === "partially_exercised") && (
          <>
            <button className="pos-card__cta pos-card__cta--ghost" disabled>
              Active · sit until expiry
            </button>
            <span className="pos-card__foot-note">
              expires in {p.dte}d — claim eligible at expiry
            </span>
          </>
        )}
        {p.status === "fully_exercised" && (
          <button className="pos-card__cta pos-card__cta--ghost" disabled>
            Fully exercised · wait for expiry to claim USDC
          </button>
        )}
      </div>
    </div>
  );
}
