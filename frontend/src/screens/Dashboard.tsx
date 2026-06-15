import { useNavigate } from "react-router-dom";
import { useDashboardState, shortAccount } from "../state/dashboard";
import { Toast } from "../components/Toast";
import { ActionModal } from "../components/ActionModal";
import { OwnedCard, WrittenCard } from "../components/PositionCards";
import { TokenLogo } from "../components/TokenLogo";
import { formatPrice } from "../format";
import type { DashboardTotals } from "../types";

function DashSummary({
  tab,
  totals,
  ownedCount,
  writtenCount,
}: {
  tab: "owned" | "written";
  totals: DashboardTotals;
  ownedCount: number;
  writtenCount: number;
}) {
  if (tab === "owned") {
    const profit = totals.ownedPnl;
    return (
      <div className="dash-summary">
        <div className="dash-summary__cell">
          <div className="dash-summary__label">Open positions</div>
          <div className="dash-summary__val">{ownedCount}</div>
          <div className="dash-summary__sub">{totals.exercisable} exercisable now</div>
        </div>
        <div className="dash-summary__cell">
          <div className="dash-summary__label">Notional</div>
          <div className="dash-summary__val">
            {formatPrice(totals.ownedNotional, { grouping: true })}
            <span className="dash-summary__unit"> USD</span>
          </div>
          <div className="dash-summary__sub">at current spot</div>
        </div>
        <div className="dash-summary__cell">
          <div className="dash-summary__label">Premium paid</div>
          <div className="dash-summary__val">
            {formatPrice(totals.ownedPaid, { grouping: true })}
            <span className="dash-summary__unit"> USDC</span>
          </div>
          <div className="dash-summary__sub">total cost basis</div>
        </div>
        <div className="dash-summary__cell">
          <div className="dash-summary__label">If exercised now</div>
          <div className={"dash-summary__val " + (profit >= 0 ? "is-pos" : "is-neg")}>
            {profit >= 0 ? "+" : "−"}
            {formatPrice(Math.abs(profit), { grouping: true })}
            <span className="dash-summary__unit"> USDC</span>
          </div>
          <div className="dash-summary__sub">net of premium paid</div>
        </div>
      </div>
    );
  }
  return (
    <div className="dash-summary">
      <div className="dash-summary__cell">
        <div className="dash-summary__label">Open positions</div>
        <div className="dash-summary__val">{writtenCount}</div>
        <div className="dash-summary__sub">{totals.claimable} claimable</div>
      </div>
      <div className="dash-summary__cell">
        <div className="dash-summary__label">Notional written</div>
        <div className="dash-summary__val">
          {formatPrice(totals.writtenNotional, { grouping: true })}
          <span className="dash-summary__unit"> USD</span>
        </div>
        <div className="dash-summary__sub">at current spot</div>
      </div>
      <div className="dash-summary__cell">
        <div className="dash-summary__label">Premium earned</div>
        <div className="dash-summary__val is-pos">
          +{formatPrice(totals.premiumEarned, { grouping: true })}
          <span className="dash-summary__unit"> USDC</span>
        </div>
        <div className="dash-summary__sub">across {writtenCount} positions</div>
      </div>
    </div>
  );
}

export function Dashboard() {
  const d = useDashboardState();
  const navigate = useNavigate();

  const empty = (label: string) => (
    <div className="dash-empty">
      <div className="dash-empty__title">no {label} yet.</div>
      <div className="dash-empty__sub">
        {label === "calls owned"
          ? "Buy a call on the Buy screen and it'll appear here."
          : "Write a covered call on the Earn screen and it'll appear here."}
      </div>
      <button
        className="dash-empty__cta"
        onClick={() => navigate(label === "calls owned" ? "/buy" : "/earn")}
      >
        Go to {label === "calls owned" ? "Buy" : "Earn"} →
      </button>
    </div>
  );

  return (
    <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
      <div className="app__wrap">
        <div className="dash-hero">
          <div className="dash-hero__eyebrow">your account</div>
          <h1 className="dash-hero__title">Dashboard</h1>
          <div className="dash-hero__addr">
            {d.connected && d.address
              ? `connected · ${shortAccount(d.address)}`
              : "not connected"}
          </div>
        </div>

        <div className="dash-tabs">
          <button
            className={"dash-tab" + (d.tab === "owned" ? " is-active" : "")}
            onClick={() => d.setTab("owned")}
          >
            <span className="dash-tab__label">Calls owned</span>
            <span className="dash-tab__count">{d.ownedRows.length}</span>
            {d.totals.exercisable > 0 && (
              <span className="dash-tab__badge">{d.totals.exercisable} exercisable</span>
            )}
          </button>
          <button
            className={"dash-tab" + (d.tab === "written" ? " is-active" : "")}
            onClick={() => d.setTab("written")}
          >
            <span className="dash-tab__label">Calls written</span>
            <span className="dash-tab__count">{d.writtenRows.length}</span>
            {d.totals.claimable > 0 && (
              <span className="dash-tab__badge">{d.totals.claimable} claimable</span>
            )}
          </button>
        </div>

        {(() => {
          const missing = Object.entries(d.spots)
            .filter(([, v]) => v === null)
            .map(([k]) => k);
          if (missing.length === 0) return null;
          return (
            <div className="dash-alert" role="alert">
              Live spot price unavailable for {missing.join(", ")}. Spot-derived
              values (notional, ITM, intrinsic) may be missing or stale until
              the feed reconnects.
            </div>
          );
        })()}

        <DashSummary
          tab={d.tab}
          totals={d.totals}
          ownedCount={d.ownedRows.length}
          writtenCount={d.writtenRows.length}
        />

        <div className="dash-list">
          {!d.connected ? (
            <div className="dash-empty">
              <div className="dash-empty__title">connect your wallet</div>
              <div className="dash-empty__sub">
                Connect a wallet to see the calls you've owned and written.
              </div>
            </div>
          ) : (
            <>
              {d.tab === "owned" && d.tradingAccountSettlements.length > 0 && (
                <div
                  className="dash-trading-account"
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: 10,
                    flexWrap: "wrap",
                    margin: "0 0 12px",
                    fontSize: 12,
                    opacity: 0.85,
                  }}
                >
                  <span style={{ opacity: 0.7 }}>in DeepBook trading account</span>
                  {d.tradingAccountSettlements.map((s) => (
                    <span
                      key={s.coinType}
                      style={{ display: "inline-flex", alignItems: "center", gap: 6 }}
                    >
                      <TokenLogo
                        symbol={s.symbol}
                        className="asset-glyph asset-glyph--sm"
                        fallback={<span className="asset-glyph">{s.symbol[0]}</span>}
                      />
                      <b>{s.amount.toLocaleString("en-US", { maximumFractionDigits: 4 })}</b> {s.symbol}
                    </span>
                  ))}
                </div>
              )}
              {d.tab === "owned" &&
                (d.ownedRows.length === 0
                  ? empty("calls owned")
                  : d.ownedRows.map((p) => (
                      <OwnedCard
                        key={p.id}
                        p={p}
                        onExercise={d.openExercise}
                        onWithdraw={d.withdrawFromTradingAccount}
                      />
                    )))}
              {d.tab === "written" &&
                (d.writtenRows.length === 0
                  ? empty("calls written")
                  : d.writtenRows.map((p) => (
                      <WrittenCard key={p.id} p={p} onClaim={d.openClaim} />
                    )))}
            </>
          )}
        </div>
      </div>

      <ActionModal
        modal={d.modal}
        spots={d.spots}
        onSubmit={d.submit}
        onClose={d.closeModal}
      />

      {d.toast && <Toast message={d.toast.message} variant={d.toast.variant} />}
    </div>
  );
}
