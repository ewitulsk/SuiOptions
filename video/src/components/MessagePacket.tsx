import { interpolate, useCurrentFrame } from "remotion";
import { colors, fonts } from "../theme";

type Props = {
  from: [number, number];
  to: [number, number];
  label: string;
  color: string;
  start: number; // frame travel begins
  duration: number; // frames in flight
  sub?: string;
};

// A labelled chip that flies from one actor to another along a straight path.
export const MessagePacket: React.FC<Props> = ({
  from,
  to,
  label,
  color,
  start,
  duration,
  sub,
}) => {
  const frame = useCurrentFrame();
  const t = interpolate(frame, [start, start + duration], [0, 1], {
    extrapolateLeft: "clamp",
    extrapolateRight: "clamp",
  });
  // Visible only while roughly in flight (small fade tails).
  const opacity = interpolate(
    frame,
    [start - 4, start + 4, start + duration - 4, start + duration + 4],
    [0, 1, 1, 0],
    { extrapolateLeft: "clamp", extrapolateRight: "clamp" },
  );

  const x = from[0] + (to[0] - from[0]) * t;
  const y = from[1] + (to[1] - from[1]) * t;

  if (opacity <= 0) return null;

  return (
    <div
      style={{
        position: "absolute",
        left: x,
        top: y,
        transform: "translate(-50%, -50%)",
        opacity,
        background: colors.bgDeep,
        border: `2px solid ${color}`,
        borderRadius: 12,
        padding: "8px 16px",
        fontFamily: fonts.mono,
        boxShadow: `0 0 24px ${color}88`,
        whiteSpace: "nowrap",
        textAlign: "center",
      }}
    >
      <div style={{ color, fontSize: 20, fontWeight: 700 }}>{label}</div>
      {sub ? (
        <div style={{ color: colors.muted, fontSize: 15, marginTop: 2 }}>
          {sub}
        </div>
      ) : null}
    </div>
  );
};
