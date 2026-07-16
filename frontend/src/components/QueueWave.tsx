import type { Bucket } from "../types";

type Props = { bucket: Bucket; amount: number; assetSymbol: string | null };

export function QueueWave({ bucket, amount, assetSymbol }: Props) {
  const asset = assetSymbol ?? "—";
  const { cursor, queued, cap } = bucket;
  const writtenStart = cursor + queued;
  const pct = (x: number) => Math.min(100, Math.max(0, (x / cap) * 100));
  return (
    <div className="cursorbar">
      <div className="cursorbar__head">
        <div className="cursorbar__title">your place in the queue</div>
        <div className="cursorbar__sub">
          cursor at <b>{cursor.toFixed(3)}</b> · you're queued{" "}
          <b>{queued.toFixed(3)} {asset}</b> behind
        </div>
      </div>
      <div className="queue-wave">
        <svg className="queue-wave__svg" viewBox="0 0 1200 24" preserveAspectRatio="none">
          <path
            className="queue-wave__wave queue-wave__wave--back"
            d="M0,12 C100,4 200,20 300,12 C400,4 500,20 600,12 C700,4 800,20 900,12 C1000,4 1100,20 1200,12"
          />
          <path
            className="queue-wave__wave"
            d="M0,14 C100,6 200,22 300,14 C400,6 500,22 600,14 C700,6 800,22 900,14 C1000,6 1100,22 1200,14"
          />
        </svg>
        <div
          className="queue-wave__zone queue-wave__zone--exercised"
          style={{ left: 0, width: `${pct(cursor)}%` }}
        />
        <div
          className="queue-wave__zone queue-wave__zone--queued"
          style={{ left: `${pct(cursor)}%`, width: `${pct(queued)}%` }}
        />
        <div
          className="queue-wave__zone queue-wave__zone--you"
          style={{ left: `${pct(writtenStart)}%`, width: `${pct(amount)}%` }}
        >
          <span className="queue-wave__you-label">you · {amount.toFixed(4)} {asset}</span>
        </div>
        <div className="queue-wave__cursor" style={{ left: `${pct(cursor)}%` }}>
          <div className="queue-wave__cursor-dot"></div>
          <span className="queue-wave__cursor-label">cursor</span>
        </div>
      </div>
      <div className="cursorbar__legend">
        <span>
          <i style={{ background: "rgba(11,94,111,0.35)" }}></i>exercised
        </span>
        <span>
          <i style={{ background: "rgba(149,170,194,0.4)" }}></i>queued ahead
        </span>
        <span>
          <i
            style={{
              background: "rgba(77,162,255,0.5)",
              border: "1.5px dashed var(--aqua-sui)",
            }}
          ></i>
          you
        </span>
      </div>
    </div>
  );
}
