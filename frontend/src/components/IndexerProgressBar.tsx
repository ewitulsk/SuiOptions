import { useIndexerProgress } from "../api/useIndexerProgress";

function formatEta(seconds: number): string {
  if (!isFinite(seconds) || seconds <= 0) return "—";
  const s = Math.round(seconds);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}

export function IndexerProgressBar() {
  const { data, isLoading, error } = useIndexerProgress();

  if (isLoading) {
    return (
      <div className="debug-card">
        <div className="debug-card__status">loading indexer status…</div>
      </div>
    );
  }
  if (error || !data) {
    return (
      <div className="debug-card">
        <div className="debug-card__status is-error">indexer status unavailable</div>
      </div>
    );
  }

  const {
    start_checkpoint,
    current_checkpoint,
    tip_checkpoint,
    rate_checkpoints_per_sec,
    caught_up,
  } = data;

  // Bar = (current − start) / (tip − start). The chain tip is the live 100%
  // target; once caught up the bar pins full and shows "live".
  const span = tip_checkpoint != null ? tip_checkpoint - start_checkpoint : null;
  const done = current_checkpoint - start_checkpoint;
  const pct =
    span && span > 0
      ? Math.max(0, Math.min(100, (done / span) * 100))
      : caught_up
        ? 100
        : 0;

  const remaining =
    tip_checkpoint != null ? Math.max(0, tip_checkpoint - current_checkpoint) : null;
  const eta =
    caught_up || remaining === 0
      ? "live"
      : remaining != null && rate_checkpoints_per_sec > 0
        ? formatEta(remaining / rate_checkpoints_per_sec)
        : "—";

  return (
    <div className="debug-card">
      <div className="debug-card__head">
        <span className="debug-card__title">Indexer sync</span>
        <span className={"debug-card__pill" + (caught_up ? " is-live" : "")}>
          {caught_up ? "live" : `${pct.toFixed(1)}%`}
        </span>
      </div>

      <div
        className="debug-bar"
        role="progressbar"
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={Math.round(pct)}
      >
        <div
          className={"debug-bar__fill" + (caught_up ? " is-live" : "")}
          style={{ width: `${pct}%` }}
        />
      </div>

      <div className="debug-stats">
        <div className="debug-stat">
          <div className="debug-stat__label">checkpoint</div>
          <div className="debug-stat__val">
            {current_checkpoint.toLocaleString()}
            {tip_checkpoint != null && (
              <span className="debug-stat__sub"> / {tip_checkpoint.toLocaleString()}</span>
            )}
          </div>
        </div>
        <div className="debug-stat">
          <div className="debug-stat__label">rate</div>
          <div className="debug-stat__val">
            {rate_checkpoints_per_sec.toFixed(1)}
            <span className="debug-stat__sub"> cp/s</span>
          </div>
        </div>
        <div className="debug-stat">
          <div className="debug-stat__label">{caught_up ? "status" : "eta to live"}</div>
          <div className="debug-stat__val">{eta}</div>
        </div>
        <div className="debug-stat">
          <div className="debug-stat__label">start</div>
          <div className="debug-stat__val">{start_checkpoint.toLocaleString()}</div>
        </div>
      </div>
    </div>
  );
}
