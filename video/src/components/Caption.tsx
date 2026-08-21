import { useCurrentFrame, useVideoConfig } from "remotion";
import { colors, fonts } from "../theme";
import { rise } from "../anim";

// Lower-third narration line shown at the bottom of a scene.
export const Caption: React.FC<{ text: string; delay?: number }> = ({
  text,
  delay = 0,
}) => {
  const frame = useCurrentFrame();
  const { fps, width } = useVideoConfig();
  const e = rise(frame, fps, delay);
  return (
    <div
      style={{
        position: "absolute",
        bottom: 70,
        left: "50%",
        transform: `translateX(-50%) translateY(${(1 - e) * 20}px)`,
        opacity: e,
        maxWidth: width - 320,
        textAlign: "center",
        fontFamily: fonts.sans,
        color: colors.muted,
        fontSize: 26,
        lineHeight: 1.4,
      }}
    >
      {text}
    </div>
  );
};

// Top-left scene label: "01 / RFQ".
export const Kicker: React.FC<{ index: string; title: string }> = ({
  index,
  title,
}) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const e = rise(frame, fps, 0);
  return (
    <div
      style={{
        position: "absolute",
        top: 56,
        left: 80,
        opacity: e,
        fontFamily: fonts.mono,
        display: "flex",
        alignItems: "center",
        gap: 14,
      }}
    >
      <span
        style={{
          color: colors.bg,
          background: colors.sui,
          borderRadius: 8,
          padding: "4px 10px",
          fontWeight: 800,
          fontSize: 22,
        }}
      >
        {index}
      </span>
      <span style={{ color: colors.text, fontSize: 24, letterSpacing: 2 }}>
        {title.toUpperCase()}
      </span>
    </div>
  );
};
