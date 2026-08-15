import { useBuckets } from "../api/useBuckets";
import { strikeKey, type Bucket, type Series } from "../api/client";

// Live view of on-chain bucket series, fetched from the Rust api-service.
// Strikes/expiry/written/cursor arrive pre-scaled (api-service joined
// against deployments.json). See rust-backend/services/api-service/README.md
// for the full shape.
export function LiveBuckets() {
  // Monitoring view: the full historical catalog rather than the listed
  // board, so it keeps showing every bucket that exists on chain (SO-400).
  const { data, isLoading, error } = useBuckets({ all: true });

  return (
    <section className="live-buckets">
      <h3 className="live-buckets__title">Live buckets</h3>
      {isLoading && <div className="live-buckets__status">loading…</div>}
      {error && (
        <div className="live-buckets__status live-buckets__status--error">
          {error.message}
        </div>
      )}
      {data && data.length === 0 && (
        <div className="live-buckets__status">no buckets indexed yet</div>
      )}
      {data && data.length > 0 && data.map((s) => <SeriesBlock key={seriesKey(s)} series={s} />)}
    </section>
  );
}

function SeriesBlock({ series }: { series: Series }) {
  return (
    <details className="live-buckets__series">
      <summary className="live-buckets__series-header">
        <span className="live-buckets__pair">
          {series.asset_symbol} / {series.settlement_symbol}
        </span>
        <span className="live-buckets__expiry">expires {formatExpiry(series.expiry_iso)}</span>
        <span className="live-buckets__count">{series.buckets.length} buckets</span>
      </summary>
      <table className="live-buckets__table">
        <thead>
          <tr>
            <th>strike</th>
            <th>written</th>
            <th>exercised</th>
            <th>fill</th>
            <th>bucket id</th>
          </tr>
        </thead>
        <tbody>
          {series.buckets.map((b) => (
            <tr key={strikeKey(b)}>
              <td>{formatStrike(b, series)}</td>
              <td>{formatAmount(b.total_written, series.asset_symbol)}</td>
              <td>{formatAmount(b.exercise_cursor, series.asset_symbol)}</td>
              <td>{formatPct(b.fill_pct)}</td>
              <td title={b.bucket_id ?? undefined}>
                {b.bucket_id ? shortHex(b.bucket_id) : "—"}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </details>
  );
}

function seriesKey(s: Series): string {
  return `${s.asset_symbol}-${s.settlement_symbol}-${s.expiry_ms}`;
}

function formatExpiry(iso: string): string {
  if (!iso) return "—";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
}

function formatStrike(b: Bucket, s: Series): string {
  if (b.strike === null) return b.strike_raw;
  return `${b.strike.toLocaleString(undefined, { maximumFractionDigits: 2 })} ${s.settlement_symbol}`;
}

function formatAmount(value: number | null, symbol: string): string {
  if (value === null) return "—";
  return `${value.toLocaleString(undefined, { maximumFractionDigits: 6 })} ${symbol}`;
}

function formatPct(value: number | null): string {
  if (value === null) return "—";
  return `${value.toFixed(1)}%`;
}

function shortHex(s: string): string {
  if (s.length <= 12) return s;
  return `${s.slice(0, 6)}…${s.slice(-4)}`;
}
