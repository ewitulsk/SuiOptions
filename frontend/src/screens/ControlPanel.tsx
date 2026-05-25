import { useRef, useEffect, useState } from "react";
import { Header } from "../components/Header";
import { WaveHero } from "../components/WaveHero";
import { useBuckets } from "../api/useBuckets";
import { useQuotingService, type RfqLogEntry } from "../api/useQuotingService";
import type { Series } from "../api/client";

function shortHex(s: string): string {
  if (s.length <= 14) return s;
  return `${s.slice(0, 6)}...${s.slice(-4)}`;
}

function BucketSelector({
  series,
  selectedBucket,
  onSelect,
}: {
  series: Series[];
  selectedBucket: string | null;
  onSelect: (bucketId: string) => void;
}) {
  return (
    <div className="cp-bucket-sel">
      {series.map((s) => (
        <div key={`${s.asset_symbol}-${s.expiry_ms}`} className="cp-bucket-sel__series">
          <div className="cp-bucket-sel__header">
            {s.asset_symbol}/{s.settlement_symbol} &middot; exp{" "}
            {new Date(s.expiry_iso).toLocaleDateString(undefined, {
              month: "short",
              day: "numeric",
            })}
          </div>
          {s.buckets.map((b) => (
            <button
              key={b.bucket_id}
              className={
                "cp-bucket-sel__btn" +
                (selectedBucket === b.bucket_id ? " is-active" : "")
              }
              onClick={() => onSelect(b.bucket_id)}
              title={b.bucket_id}
            >
              strike{" "}
              {b.strike !== null
                ? b.strike.toLocaleString(undefined, { maximumFractionDigits: 2 })
                : b.strike_raw}{" "}
              {s.settlement_symbol} &middot; {shortHex(b.bucket_id)}
            </button>
          ))}
        </div>
      ))}
    </div>
  );
}

function LogRow({ entry }: { entry: RfqLogEntry }) {
  const time = new Date(entry.ts).toLocaleTimeString();
  const arrow = entry.direction === "out" ? "\u2192" : "\u2190";

  let detail: string;
  if (entry.kind === "request") {
    const p = entry.payload as { bucket_id: string; side: string; write_amount: string };
    detail = `RFQ ${p.side} bucket=${shortHex(p.bucket_id)} amt=${p.write_amount}`;
  } else if (entry.kind === "response") {
    const p = entry.payload as { quotes: unknown[]; bucket_id: string };
    detail = `Response bucket=${shortHex(p.bucket_id)} quotes=${p.quotes.length}`;
  } else {
    const p = entry.payload as { code: string; message: string };
    detail = `Error [${p.code}] ${p.message}`;
  }

  return (
    <div className={`cp-log__row cp-log__row--${entry.kind}`}>
      <span className="cp-log__time">{time}</span>
      <span className="cp-log__arrow">{arrow}</span>
      <span className="cp-log__rid">{entry.requestId}</span>
      <span className="cp-log__detail">{detail}</span>
    </div>
  );
}

export function ControlPanel() {
  const { data: series, isLoading } = useBuckets();
  const { connected, sessionId, log, sendRfq, clearLog } =
    useQuotingService();

  const [selectedBucket, setSelectedBucket] = useState<string | null>(null);
  const [side, setSide] = useState<"writer" | "trader">("writer");
  const [writeAmount, setWriteAmount] = useState("100000000");
  const [autoFire, setAutoFire] = useState(false);

  const logEndRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    logEndRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [log.length]);

  // Auto-fire: send an RFQ every 60s for each bucket in every series.
  useEffect(() => {
    if (!autoFire || !connected || !series || series.length === 0) return;

    const fire = () => {
      for (const s of series) {
        for (const b of s.buckets) {
          sendRfq({ bucket_id: b.bucket_id, write_amount: writeAmount, side });
        }
      }
    };
    fire();
    const interval = setInterval(fire, 60_000);
    return () => clearInterval(interval);
  }, [autoFire, connected, series, sendRfq, side, writeAmount]);

  const handleManualRfq = () => {
    if (!selectedBucket) return;
    sendRfq({ bucket_id: selectedBucket, write_amount: writeAmount, side });
  };

  return (
    <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
      <WaveHero />
      <Header />

      <div className="app__wrap">
        <h2 className="cp-title">Control Panel &mdash; RFQ Pipeline</h2>

        <div className="cp-status">
          <span className={connected ? "cp-status__dot cp-status__dot--on" : "cp-status__dot"} />
          {connected ? `Connected (session ${shortHex(sessionId ?? "")})` : "Disconnected"}
        </div>

        <div className="cp-grid">
          <section className="cp-section">
            <h3>Buckets</h3>
            {isLoading && <div className="cp-muted">loading buckets...</div>}
            {series && series.length === 0 && (
              <div className="cp-muted">no buckets indexed yet</div>
            )}
            {series && series.length > 0 && (
              <BucketSelector
                series={series}
                selectedBucket={selectedBucket}
                onSelect={setSelectedBucket}
              />
            )}
          </section>

          <section className="cp-section">
            <h3>Send RFQ</h3>
            <div className="cp-form">
              <label className="cp-label">
                Side
                <select
                  className="cp-input"
                  value={side}
                  onChange={(e) => setSide(e.target.value as "writer" | "trader")}
                >
                  <option value="writer">Writer</option>
                  <option value="trader">Trader</option>
                </select>
              </label>
              <label className="cp-label">
                Write amount (raw)
                <input
                  className="cp-input"
                  type="text"
                  value={writeAmount}
                  onChange={(e) => setWriteAmount(e.target.value)}
                />
              </label>
              <div className="cp-actions">
                <button
                  className="cta cp-btn"
                  disabled={!connected || !selectedBucket}
                  onClick={handleManualRfq}
                >
                  Send RFQ
                </button>
                <label className="cp-auto">
                  <input
                    type="checkbox"
                    checked={autoFire}
                    onChange={(e) => setAutoFire(e.target.checked)}
                  />
                  Auto-fire every 60s (all buckets)
                </label>
              </div>
            </div>
          </section>
        </div>

        <section className="cp-section cp-log-section">
          <div className="cp-log-header">
            <h3>RFQ Log</h3>
            <button className="cp-btn cp-btn--small" onClick={clearLog}>
              Clear
            </button>
          </div>
          <div className="cp-log">
            {log.length === 0 && (
              <div className="cp-muted" style={{ padding: 16, textAlign: "center" }}>
                no RFQs yet &mdash;{" "}
                {connected
                  ? "select a bucket and send an RFQ, or enable auto-fire"
                  : "waiting for WS connection..."}
              </div>
            )}
            {log.map((entry) => (
              <LogRow key={entry.id} entry={entry} />
            ))}
            <div ref={logEndRef} />
          </div>
        </section>
      </div>
    </div>
  );
}
