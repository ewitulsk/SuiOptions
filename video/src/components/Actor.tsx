import { useCurrentFrame, useVideoConfig } from "remotion";
import { colors, fonts } from "../theme";
import { rise } from "../anim";

type Props = {
  icon: string;
  title: string;
  subtitle?: string;
  accent: string;
  x: number;
  y: number;
  width?: number;
  delay?: number;
  highlight?: number; // 0..1 glow strength
};

// A labelled participant box (Writer / Quoting Service / Trader MM / Chain).
export const Actor: React.FC<Props> = ({
  icon,
  title,
  subtitle,
  accent,
  x,
  y,
  width = 300,
  delay = 0,
  highlight = 0,
}) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const e = rise(frame, fps, delay);

  return (
    <div
      style={{
        position: "absolute",
        left: x,
        top: y,
        width,
        transform: `translate(-50%, -50%) scale(${0.85 + e * 0.15})`,
        opacity: e,
        background: `linear-gradient(180deg, ${colors.panelLight}, ${colors.panel})`,
        border: `2px solid ${accent}`,
        borderRadius: 20,
        padding: "22px 26px",
        boxShadow: `0 20px 60px #0008, 0 0 ${20 + highlight * 60}px ${accent}${highlight > 0 ? "aa" : "33"}`,
        fontFamily: fonts.sans,
      }}
    >
      <div style={{ display: "flex", alignItems: "center", gap: 16 }}>
        <div
          style={{
            fontSize: 44,
            width: 64,
            height: 64,
            borderRadius: 14,
            background: `${accent}22`,
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
          }}
        >
          {icon}
        </div>
        <div>
          <div style={{ color: colors.text, fontSize: 28, fontWeight: 700 }}>
            {title}
          </div>
          {subtitle ? (
            <div style={{ color: colors.muted, fontSize: 18, marginTop: 4 }}>
              {subtitle}
            </div>
          ) : null}
        </div>
      </div>
    </div>
  );
};
