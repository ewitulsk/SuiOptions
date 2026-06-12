import { useCurrentAccount } from "@mysten/dapp-kit";
import {
  ACTIVITY_FILTERS,
  EVENT_TYPE_META,
  formatDay,
  relativeTime,
  useActivityState,
  type ActivityFilter,
} from "../state/activity";
import { formatPrice } from "../format";
import type { ActivityEvent, ActivityTotals } from "../types";

function shortAddress(addr: string): string {
  const s = addr.startsWith("0x") ? addr.slice(2) : addr;
  if (s.length <= 8) return `0x${s}`;
  return `0x${s.slice(0, 4)}…${s.slice(-4)}`;
}

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
          +{formatPrice(totals.premiumIn, { grouping: true })} USDC premium received
        </div>
      </div>
      <div className="dash-summary__cell">
        <div className="dash-summary__label">Calls bought</div>
        <div className="dash-summary__val">{totals.buys}</div>
        <div className="dash-summary__sub">
          −{formatPrice(totals.premiumOut, { grouping: true })} USDC premium paid
        </div>
      </div>
      <div className="dash-summary__cell">
        <div className="dash-summary__label">Net premium</div>
        <div className={"dash-summary__val " + (net >= 0 ? "is-pos" : "is-neg")}>
          {net >= 0 ? "+" : "−"}
          {formatPrice(Math.abs(net), { grouping: true })}
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
            {e.value.unit === "BTC"
              ? Math.abs(e.value.delta).toLocaleString("en-US", { maximumFractionDigits: 4 })
              : formatPrice(Math.abs(e.value.delta), { grouping: true })}
            <span className="act-row__delta-unit"> {e.value.unit}</span>
          </div>
        )}
      </div>
    </div>
  );
}

export function Activity() {
  const account = useCurrentAccount();
  const wallet = account?.address ?? null;
  const a = useActivityState(wallet);
  const earliest = a.events[a.events.length - 1];

  return (
    <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
      <div className="app__wrap">
        <div className="dash-hero">
          <div className="dash-hero__eyebrow">on-chain log · indexer-backed</div>
          <h1 className="dash-hero__title">Activity</h1>
          <div className="dash-hero__addr">
            {wallet
              ? `every event for ${shortAddress(wallet)} · polled from the indexer`
              : "connect your wallet to see your activity"}
          </div>
        </div>

        {!wallet ? (
          <div className="act-empty">
            <div className="act-empty__title">connect your wallet</div>
            <div className="act-empty__sub">
              Your on-chain activity appears here once a wallet is connected.
            </div>
          </div>
        ) : (
          <>
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
          </>
        )}
      </div>
    </div>
  );
}
