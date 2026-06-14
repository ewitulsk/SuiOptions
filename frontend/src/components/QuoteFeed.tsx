import { formatPrice } from "../format";
import type { Quote, View } from "../types";

// `docked` renders the feed inline inside the RFQ section (SO-170) rather than
// as the floating bottom-right overlay; the close button is omitted when no
// `onClose` is supplied.
type Props = { quotes: Quote[]; view: View; onClose?: () => void; docked?: boolean };

export function QuoteFeed({ quotes, view, onClose, docked }: Props) {
  return (
    <div className={"feed" + (docked ? " feed--docked" : "")}>
      <div className="feed__head">
        <span className="feed__title">
          {view === "writer" ? "bids · ranked high → low" : "asks · ranked low → high"}
        </span>
        {onClose && (
          <button className="feed__close" onClick={onClose}>
            ×
          </button>
        )}
      </div>
      <div className="feed__list">
        {quotes.length === 0 && (
          <div
            style={{
              padding: "20px 14px",
              textAlign: "center",
              fontFamily: "JetBrains Mono, monospace",
              fontSize: 11,
              color: "var(--aqua-ink-3)",
            }}
          >
            waiting on MMs…
          </div>
        )}
        {quotes.map((q, i) => (
          <div key={q.id} className={"qrow" + (i === 0 ? " is-best" : "")}>
            <div>
              <div className="qrow__name">{q.name}</div>
              <div className="qrow__sub">
                {q.addr} · fill {q.fill}% · {q.latency}ms
              </div>
            </div>
            <div>
              <div className="qrow__premium">{formatPrice(q.premium)}</div>
              <div className="qrow__ttl">ttl {q.ttl}s</div>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}
