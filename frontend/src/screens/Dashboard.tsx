import { useNavigate } from "react-router-dom";
import { useDashboardState } from "../mocks/dashboard";
import { Header } from "../components/Header";
import { WaveHero } from "../components/WaveHero";
import { Toast } from "../components/Toast";
import { ActionModal } from "../components/ActionModal";
import { OwnedCard, WrittenCard } from "../components/PositionCards";
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
            {totals.ownedNotional.toLocaleString("en-US", { maximumFractionDigits: 0 })}
            <span className="dash-summary__unit"> USD</span>
          </div>
          <div className="dash-summary__sub">at current spot</div>
        </div>
        <div className="dash-summary__cell">
          <div className="dash-summary__label">Premium paid</div>
          <div className="dash-summary__val">
            {totals.ownedPaid.toFixed(2)}
            <span className="dash-summary__unit"> USDC</span>
          </div>
          <div className="dash-summary__sub">total cost basis</div>
        </div>
        <div className="dash-summary__cell">
          <div className="dash-summary__label">If exercised now</div>
          <div className={"dash-summary__val " + (profit >= 0 ? "is-pos" : "is-neg")}>
            {profit >= 0 ? "+" : "−"}
            {Math.abs(profit).toFixed(2)}
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
          {totals.writtenNotional.toLocaleString("en-US", { maximumFractionDigits: 0 })}
          <span className="dash-summary__unit"> USD</span>
        </div>
        <div className="dash-summary__sub">at current spot</div>
      </div>
      <div className="dash-summary__cell">
        <div className="dash-summary__label">Premium earned</div>
        <div className="dash-summary__val is-pos">
          +{totals.premiumEarned.toFixed(2)}
          <span className="dash-summary__unit"> USDC</span>
        </div>
        <div className="dash-summary__sub">across {writtenCount} positions</div>
      </div>
      <div className="dash-summary__cell">
        <div className="dash-summary__label">Avg fill latency</div>
        <div className="dash-summary__val">
          142<span className="dash-summary__unit"> ms</span>
        </div>
        <div className="dash-summary__sub">MM response time</div>
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
      <WaveHero />
      <Header />

      <div className="app__wrap">
        <div className="dash-hero">
          <div className="dash-hero__eyebrow">your account</div>
          <h1 className="dash-hero__title">Dashboard</h1>
          <div className="dash-hero__addr">connected · 0x9f3a…42b1</div>
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
          {d.tab === "owned" &&
            (d.ownedRows.length === 0
              ? empty("calls owned")
              : d.ownedRows.map((p) => (
                  <OwnedCard key={p.id} p={p} onExercise={d.openExercise} />
                )))}
          {d.tab === "written" &&
            (d.writtenRows.length === 0
              ? empty("calls written")
              : d.writtenRows.map((p) => (
                  <WrittenCard
                    key={p.id}
                    p={p}
                    onClaim={d.openClaim}
                    onCloseEarly={d.openCloseEarly}
                  />
                )))}
        </div>
      </div>

      <ActionModal
        modal={d.modal}
        spots={d.spots}
        onSubmit={d.submit}
        onClose={d.closeModal}
      />

      {d.toast && <Toast message={d.toast} />}
    </div>
  );
}
