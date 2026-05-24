import {
  ACTIVITY_FILTERS,
  EVENT_TYPE_META,
  formatDay,
  relativeTime,
  useActivityState,
  type ActivityFilter,
} from "../mocks/activity";
import { Header } from "../components/Header";
import { WaveHero } from "../components/WaveHero";
import type { ActivityEvent, ActivityTotals } from "../types";

function ActivitySummary({ totals }: { totals: ActivityTotals }) {
  const net = totals.premiumIn - totals.premiumOut;
  return (
    <div className="dash-summary act-summary">
      <div className="dash-summary__cell">
        <div className="dash-summary__label">Lifetime exercises</div>
        <div className="dash-summary__val">{totals.exercises}</div>
        <div className="dash-summary__sub">across owned calls</div>
      </div>
      <div className="dash-summary__cell">
        <div className="dash-summary__label">Calls written</div>
        <div className="dash-summary__val">{totals.writes}</div>
        <div className="dash-summary__sub">
          +{totals.premiumIn.toFixed(2)} USDC premium received
        </div>
      </div>
      <div className="dash-summary__cell">
        <div className="dash-summary__label">Calls bought</div>
        <div className="dash-summary__val">{totals.buys}</div>
        <div className="dash-summary__sub">
          −{totals.premiumOut.toFixed(2)} USDC premium paid
        </div>
      </div>
      <div className="dash-summary__cell">
        <div className="dash-summary__label">Net premium</div>
        <div className={"dash-summary__val " + (net >= 0 ? "is-pos" : "is-neg")}>
          {net >= 0 ? "+" : "−"}
          {Math.abs(net).toFixed(2)}
          <span className="dash-summary__unit"> USDC</span>
        </div>
        <div className="dash-summary__sub">lifetime · written − bought</div>
      </div>
    </div>
  );
}

function ActivityFilters({
  filter,
  setFilter,
}: {
  filter: ActivityFilter;
  setFilter: (f: ActivityFilter) => void;
}) {
  return (
    <div className="act-filters">
      {ACTIVITY_FILTERS.map((f) => (
        <button
          key={f.key}
          className={"act-filter" + (filter === f.key ? " is-active" : "")}
          onClick={() => setFilter(f.key)}
        >
          {f.label}
        </button>
      ))}
    </div>
  );
}

function EventRow({ e, now }: { e: ActivityEvent; now: number }) {
  const meta = EVENT_TYPE_META[e.type] ?? { label: e.type, icon: "·", accent: "mute" as const };
  const isNeg = e.value && e.value.delta < 0;
  const isPos = e.value && e.value.delta > 0;
  return (
    <div className={"act-row act-row--" + meta.accent + " act-row--status-" + e.status}>
      <div className="act-row__time">
        <div className="act-row__time-rel">{relativeTime(e.ts, now)}</div>
        <div className="act-row__time-abs">
          {new Date(e.ts).toLocaleTimeString("en-US", {
            hour: "numeric",
            minute: "2-digit",
          })}
        </div>
      </div>
      <div className={"act-row__icon act-row__icon--" + meta.accent}>
        <span>{meta.icon}</span>
      </div>
      <div className="act-row__body">
        <div className="act-row__title-row">
          <span className="act-row__type">{meta.label}</span>
          {e.bucket && <span className="act-row__bucket">{e.bucket}</span>}
          {e.side && e.side !== "account" && (
            <span className={"act-row__side act-row__side--" + e.side}>{e.side}</span>
          )}
          {e.status === "pending" && (
            <span className="act-row__status act-row__status--pending">pending</span>
          )}
          {e.status === "expired" && (
            <span className="act-row__status act-row__status--expired">expired</span>
          )}
          {e.status === "reverted" && (
            <span className="act-row__status act-row__status--reverted">reverted</span>
          )}
        </div>
        <div className="act-row__title">{e.title}</div>
        <div className="act-row__body-text">{e.body}</div>
        {e.txHash && (
          <div className="act-row__tx">
            <span className="act-row__tx-label">tx</span>
            <code className="act-row__tx-hash">{e.txHash}</code>
          </div>
        )}
      </div>
      <div className="act-row__value">
        {e.value && (
          <div className={"act-row__delta " + (isPos ? "is-pos" : isNeg ? "is-neg" : "")}>
            {isPos ? "+" : isNeg ? "−" : ""}
            {Math.abs(e.value.delta).toLocaleString("en-US", {
              maximumFractionDigits: e.value.unit === "BTC" ? 4 : 2,
            })}
            <span className="act-row__delta-unit"> {e.value.unit}</span>
          </div>
        )}
      </div>
    </div>
  );
}

export function Activity() {
  const a = useActivityState();
  const earliest = a.events[a.events.length - 1];

  return (
    <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
      <WaveHero />
      <Header />

      <div className="app__wrap">
        <div className="dash-hero">
          <div className="dash-hero__eyebrow">on-chain log · indexer-backed</div>
          <h1 className="dash-hero__title">Activity</h1>
          <div className="dash-hero__addr">
            every event for 0x9f3a…42b1 · live tail via wss
          </div>
        </div>

        <ActivitySummary totals={a.totals} />
        <ActivityFilters filter={a.filter} setFilter={a.setFilter} />

        <div className="act-timeline">
          {a.grouped.length === 0 && (
            <div className="act-empty">
              <div className="act-empty__title">no events match this filter.</div>
              <div className="act-empty__sub">
                try "All activity" — there are {a.events.length} events on file.
              </div>
            </div>
          )}
          {a.grouped.map((row) =>
            row.kind === "day" ? (
              <div key={row.key} className="act-day">
                <span className="act-day__label">{formatDay(row.date)}</span>
                <span className="act-day__rule"></span>
                <span className="act-day__abs">
                  {row.date.toLocaleDateString("en-US", {
                    year: "numeric",
                    month: "short",
                    day: "numeric",
                  })}
                </span>
              </div>
            ) : (
              <EventRow key={row.e.id} e={row.e} now={a.now} />
            ),
          )}
        </div>

        {earliest && (
          <div className="act-foot">
            end of history · earliest event{" "}
            {new Date(earliest.ts).toLocaleDateString("en-US", {
              year: "numeric",
              month: "short",
              day: "numeric",
            })}
          </div>
        )}
      </div>
    </div>
  );
}
