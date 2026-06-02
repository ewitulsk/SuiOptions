import { AbsoluteFill, interpolate, useCurrentFrame, useVideoConfig } from "remotion";
import { Caption, Kicker } from "../components/Caption";
import { colors, fonts } from "../theme";
import { rise, fade } from "../anim";

const BAR_X = 200;
const BAR_W = 1520;
const BAR_Y = 430;
const BAR_H = 90;
const MAX = 100;
const px = (units: number) => BAR_X + (units / MAX) * BAR_W;

const SEGMENTS = [
  { a: 0, b: 25, label: "Writer A", note: "wrote early · low premium" },
  { a: 25, b: 45, label: "Writer B", note: "" },
  { a: 45, b: 60, label: "You", note: "wrote mid · range [45, 60)", you: true },
  { a: 60, b: 100, label: "Writer D", note: "wrote late · high premium" },
];

const CURSOR_END = 52;

export const CursorScene: React.FC = () => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const enter = rise(frame, fps, 4);

  // Cursor sweeps FIFO from 0 to CURSOR_END.
  const cursor = interpolate(frame, [40, 130], [0, CURSOR_END], {
    extrapolateLeft: "clamp",
    extrapolateRight: "clamp",
  });
  const cursorX = px(cursor);

  return (
    <AbsoluteFill>
      <Kicker index="05" title="The cursor model" />

      <div
        style={{
          position: "absolute",
          top: 150,
          width: "100%",
          textAlign: "center",
          fontFamily: fonts.sans,
          color: colors.text,
          fontSize: 34,
          fontWeight: 600,
          opacity: fade(frame, 4, 24),
        }}
      >
        All writers share one bucket. Each write claims a slice of a number line.
      </div>

      {/* The number line */}
      <div
        style={{
          position: "absolute",
          left: BAR_X,
          top: BAR_Y,
          width: BAR_W,
          height: BAR_H,
          opacity: enter,
          borderRadius: 12,
          background: colors.bgDeep,
          border: `1px solid ${colors.stroke}`,
        }}
      />

      {SEGMENTS.map((s) => {
        const x = px(s.a);
        const w = px(s.b) - px(s.a);
        const e = fade(frame, 8 + s.a * 0.2, 24 + s.a * 0.2);
        return (
          <div key={s.label}>
            <div
              style={{
                position: "absolute",
                left: x + 4,
                top: BAR_Y + 4,
                width: w - 8,
                height: BAR_H - 8,
                opacity: e,
                borderRadius: 8,
                background: s.you ? `${colors.teal}33` : `${colors.sui}22`,
                border: `2px solid ${s.you ? colors.teal : colors.sui}`,
                display: "flex",
                alignItems: "center",
                justifyContent: "center",
                color: s.you ? colors.teal : colors.text,
                fontFamily: fonts.sans,
                fontWeight: 700,
                fontSize: 22,
              }}
            >
              {s.label}
            </div>
            <div
              style={{
                position: "absolute",
                left: x + w / 2,
                top: BAR_Y + BAR_H + 14,
                transform: "translateX(-50%)",
                opacity: e * 0.9,
                color: colors.muted,
                fontFamily: fonts.mono,
                fontSize: 16,
                whiteSpace: "nowrap",
                textAlign: "center",
              }}
            >
              [{s.a}, {s.b}){s.note ? ` · ${s.note}` : ""}
            </div>
          </div>
        );
      })}

      {/* Exercised overlay sweeping left-to-right */}
      <div
        style={{
          position: "absolute",
          left: BAR_X + 4,
          top: BAR_Y + 4,
          width: Math.max(0, cursorX - BAR_X - 4),
          height: BAR_H - 8,
          borderRadius: 8,
          background: `repeating-linear-gradient(45deg, ${colors.amber}55, ${colors.amber}55 8px, ${colors.amber}22 8px, ${colors.amber}22 16px)`,
          opacity: fade(frame, 38, 50),
        }}
      />

      {/* The cursor itself */}
      <div
        style={{
          position: "absolute",
          left: cursorX,
          top: BAR_Y - 36,
          height: BAR_H + 72,
          width: 0,
          borderLeft: `4px solid ${colors.amber}`,
          opacity: fade(frame, 38, 50),
        }}
      />
      <div
        style={{
          position: "absolute",
          left: cursorX,
          top: BAR_Y - 70,
          transform: "translateX(-50%)",
          color: colors.amber,
          fontFamily: fonts.mono,
          fontWeight: 700,
          fontSize: 20,
          opacity: fade(frame, 38, 50),
          whiteSpace: "nowrap",
        }}
      >
        exercise_cursor ▸ {Math.round(cursor)}
      </div>

      {/* Redeem math for "You" */}
      <RedeemCard delay={150} />

      <Caption
        text="Exercises advance one shared cursor in write-order (FIFO). At expiry your payout is just your range vs. the cursor: the covered part pays settlement at strike, the rest returns your underlying — an O(1) cursor instead of touching every position."
        delay={170}
      />
    </AbsoluteFill>
  );
};

const RedeemCard: React.FC<{ delay: number }> = ({ delay }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const e = rise(frame, fps, delay);
  return (
    <div
      style={{
        position: "absolute",
        left: "50%",
        top: 660,
        transform: `translateX(-50%) scale(${0.94 + e * 0.06})`,
        opacity: e,
        display: "flex",
        gap: 24,
        fontFamily: fonts.mono,
      }}
    >
      <Stat color={colors.amber} big="7" label="exercised  [45 → 52)" sub="paid in USDC at strike" />
      <Stat color={colors.teal} big="8" label="unexercised  [52 → 60)" sub="wBTC returned to you" />
    </div>
  );
};

const Stat: React.FC<{ color: string; big: string; label: string; sub: string }> = ({
  color,
  big,
  label,
  sub,
}) => (
  <div
    style={{
      background: colors.panel,
      border: `1px solid ${color}`,
      borderRadius: 16,
      padding: "16px 26px",
      minWidth: 320,
      textAlign: "center",
    }}
  >
    <div style={{ color, fontSize: 40, fontWeight: 800 }}>{big}</div>
    <div style={{ color: colors.text, fontSize: 20, marginTop: 2 }}>{label}</div>
    <div style={{ color: colors.muted, fontSize: 16, marginTop: 4 }}>{sub}</div>
  </div>
);
