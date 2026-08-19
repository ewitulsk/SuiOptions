// §3.3 coverage gauge (SO-418): a margin-style gauge for the junior buffer
// ratio (junior NAV / total NAV) against the vault's two immutable
// thresholds — red below maintenance (coverage breach), amber below target
// (no new senior deposits), green healthy. `compact` renders inline on list
// rows; `full` adds threshold labels and the "what this means" copy from
// docs/trading-vault-v2/disclosures.md.

const RED = "var(--aqua-down, #e05555)";
const AMBER = "#d99a2b";
const GREEN = "var(--aqua-up, #1fbf75)";

function zoneOf(bufferBps: number, targetBps: number, maintenanceBps: number) {
  return bufferBps < maintenanceBps ? "breach" : bufferBps < targetBps ? "belowTarget" : "healthy";
}

const ZONE_COLOR = { breach: RED, belowTarget: AMBER, healthy: GREEN } as const;

// Disclosure copy — keep in sync with disclosures.md "Tranches" section.
const ZONE_COPY = {
  healthy:
    "Healthy: the junior buffer is at or above the target. Senior deposits and junior withdrawals both flow normally.",
  belowTarget:
    "Below target: new senior deposits stop, but nothing else changes.",
  breach:
    "Coverage breach: junior withdrawals pause (they stay queued in order); senior withdrawals keep flowing; the curator can only unwind, not deploy; new senior deposits stop.",
} as const;

function Track({
  bufferBps,
  targetBps,
  maintenanceBps,
  scaleMaxBps,
  height,
}: {
  bufferBps: number;
  targetBps: number;
  maintenanceBps: number;
  scaleMaxBps: number;
  height: number;
}) {
  const pct = (bps: number) => Math.min(100, Math.max(0, (bps / scaleMaxBps) * 100));
  const zone = zoneOf(bufferBps, targetBps, maintenanceBps);
  return (
    <span
      style={{
        position: "relative",
        display: "block",
        height,
        borderRadius: height / 2,
        overflow: "hidden",
        background:
          // Threshold zones as a single gradient: red → maintenance,
          // amber → target, green above.
          `linear-gradient(to right,
            rgba(224,85,85,0.28) 0 ${pct(maintenanceBps)}%,
            rgba(217,154,43,0.28) ${pct(maintenanceBps)}% ${pct(targetBps)}%,
            rgba(31,191,117,0.28) ${pct(targetBps)}% 100%)`,
      }}
    >
      {/* Filled portion up to the current buffer, toned by zone. */}
      <span
        style={{
          position: "absolute",
          inset: 0,
          width: `${pct(bufferBps)}%`,
          background: ZONE_COLOR[zone],
          opacity: 0.85,
          transition: "width .3s ease",
        }}
      />
      {/* Threshold ticks. */}
      {[maintenanceBps, targetBps].map((t) => (
        <span
          key={t}
          style={{
            position: "absolute",
            top: 0,
            bottom: 0,
            left: `${pct(t)}%`,
            width: 2,
            background: "var(--aqua-ink-1, #1c2733)",
            opacity: 0.55,
          }}
        />
      ))}
    </span>
  );
}

export function CoverageGauge({
  bufferBps,
  targetBps,
  maintenanceBps,
  variant,
}: {
  /** junior NAV × 10⁴ / NAV from the latest sync; null before it. */
  bufferBps: number | null;
  targetBps: number;
  maintenanceBps: number;
  variant: "compact" | "full";
}) {
  // The scale runs to 2× target (or a bit past the buffer when it's higher)
  // so both thresholds always land inside the track.
  const scaleMaxBps = Math.max(targetBps * 2, Math.ceil((bufferBps ?? 0) * 1.15), 1);

  if (variant === "compact") {
    if (bufferBps == null) return null;
    const zone = zoneOf(bufferBps, targetBps, maintenanceBps);
    return (
      <span
        style={{ display: "inline-flex", alignItems: "center", gap: 6 }}
        title={`Junior buffer ${(bufferBps / 100).toFixed(2)}% · target ${(targetBps / 100).toFixed(2)}% · maintenance ${(maintenanceBps / 100).toFixed(2)}%`}
      >
        <span style={{ width: 54, flex: "0 0 auto" }}>
          <Track
            bufferBps={bufferBps}
            targetBps={targetBps}
            maintenanceBps={maintenanceBps}
            scaleMaxBps={scaleMaxBps}
            height={6}
          />
        </span>
        <span style={{ fontSize: 10, color: ZONE_COLOR[zone] }}>
          {(bufferBps / 100).toFixed(1)}%
        </span>
      </span>
    );
  }

  const zone = bufferBps != null ? zoneOf(bufferBps, targetBps, maintenanceBps) : null;
  const pctLabel = (bps: number) => `${(bps / 100).toFixed(2)}%`;
  return (
    <div>
      <div
        style={{
          display: "flex",
          alignItems: "baseline",
          gap: 8,
          marginBottom: 6,
          fontSize: 12,
        }}
      >
        <span>Junior buffer</span>
        <b style={{ fontSize: 18, color: zone != null ? ZONE_COLOR[zone] : undefined }}>
          {bufferBps != null ? pctLabel(bufferBps) : "—"}
        </b>
        <span className="vault-bids__sub" style={{ marginLeft: "auto" }}>
          junior NAV / total NAV
        </span>
      </div>
      {bufferBps != null ? (
        <Track
          bufferBps={bufferBps}
          targetBps={targetBps}
          maintenanceBps={maintenanceBps}
          scaleMaxBps={scaleMaxBps}
          height={12}
        />
      ) : (
        <div className="vault-prose__muted" style={{ fontSize: 11 }}>
          No capital sync yet — the gauge fills in after the first consumed
          appraisal.
        </div>
      )}
      <div
        style={{
          display: "flex",
          justifyContent: "space-between",
          fontSize: 10,
          marginTop: 4,
        }}
        className="vault-prose__muted"
      >
        <span style={{ color: RED }}>maintenance {pctLabel(maintenanceBps)}</span>
        <span style={{ color: AMBER }}>target {pctLabel(targetBps)}</span>
      </div>
      <div className="vault-prose__muted" style={{ fontSize: 11, marginTop: 6 }}>
        {zone != null ? ZONE_COPY[zone] : null} This is how much loss junior
        can absorb before senior is exposed — both thresholds are immutable
        for the life of the vault.
      </div>
    </div>
  );
}
