import { AbsoluteFill, useCurrentFrame, useVideoConfig } from "remotion";
import { colors, fonts } from "../theme";
import { rise, fade } from "../anim";

const STEPS = [
  { n: "1", t: "Request", d: "Writer asks; service fans out the RFQ" },
  { n: "2", t: "Quote", d: "MMs sign; service validates & reserves" },
  { n: "3", t: "Fill", d: "execute_write settles it atomically" },
  { n: "4", t: "Outcome", d: "Position NFT to writer · CallOption to MM" },
];

export const OutroScene: React.FC = () => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const head = rise(frame, fps, 4);

  return (
    <AbsoluteFill style={{ fontFamily: fonts.sans }}>
      <div
        style={{
          position: "absolute",
          top: 150,
          width: "100%",
          textAlign: "center",
          opacity: head,
          color: colors.text,
          fontSize: 52,
          fontWeight: 800,
        }}
      >
        The write flow, end to end
      </div>

      <div
        style={{
          position: "absolute",
          top: 320,
          left: "50%",
          transform: "translateX(-50%)",
          display: "flex",
          gap: 28,
        }}
      >
        {STEPS.map((s, i) => {
          const e = rise(frame, fps, 18 + i * 12);
          return (
            <div
              key={s.n}
              style={{
                opacity: e,
                transform: `translateY(${(1 - e) * 24}px)`,
                width: 340,
                background: colors.panel,
                border: `1px solid ${colors.stroke}`,
                borderRadius: 18,
                padding: "26px 24px",
              }}
            >
              <div
                style={{
                  width: 48,
                  height: 48,
                  borderRadius: 12,
                  background: colors.sui,
                  color: colors.bg,
                  fontWeight: 800,
                  fontSize: 26,
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "center",
                  marginBottom: 16,
                }}
              >
                {s.n}
              </div>
              <div style={{ color: colors.teal, fontSize: 26, fontWeight: 700 }}>{s.t}</div>
              <div style={{ color: colors.muted, fontSize: 19, marginTop: 8, lineHeight: 1.4 }}>
                {s.d}
              </div>
            </div>
          );
        })}
      </div>

      <div
        style={{
          position: "absolute",
          top: 640,
          width: "100%",
          textAlign: "center",
          opacity: fade(frame, 80, 100),
          color: colors.muted,
          fontSize: 24,
          lineHeight: 1.5,
        }}
      >
        After expiry: the <span style={{ color: colors.amber }}>CallOption</span> holder can{" "}
        <b style={{ color: colors.text }}>exercise</b>, and the{" "}
        <span style={{ color: colors.teal }}>Position</span> holder can{" "}
        <b style={{ color: colors.text }}>redeem</b> — settled by the same cursor.
      </div>

      <div
        style={{
          position: "absolute",
          bottom: 90,
          width: "100%",
          textAlign: "center",
          opacity: fade(frame, 100, 120),
          color: colors.text,
          fontSize: 40,
          fontWeight: 800,
        }}
      >
        SuiOptions <span style={{ color: colors.sui }}>·</span>{" "}
        <span style={{ color: colors.muted, fontWeight: 500, fontSize: 28 }}>
          pooled buckets, FIFO by cursor
        </span>
      </div>
    </AbsoluteFill>
  );
};
