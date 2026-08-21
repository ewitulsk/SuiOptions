import { AbsoluteFill, useCurrentFrame } from "remotion";
import { colors } from "../theme";

// Animated grid + soft glow backdrop shared by every scene.
export const Background: React.FC = () => {
  const frame = useCurrentFrame();
  const drift = (frame * 0.25) % 64;

  return (
    <AbsoluteFill style={{ backgroundColor: colors.bg }}>
      <AbsoluteFill
        style={{
          background: `radial-gradient(1200px 700px at 50% -10%, ${colors.panelLight}55, transparent 60%), radial-gradient(900px 600px at 90% 110%, ${colors.sui}22, transparent 60%)`,
        }}
      />
      <AbsoluteFill
        style={{
          opacity: 0.35,
          backgroundImage: `linear-gradient(${colors.stroke}33 1px, transparent 1px), linear-gradient(90deg, ${colors.stroke}33 1px, transparent 1px)`,
          backgroundSize: "64px 64px",
          backgroundPosition: `${drift}px ${drift}px`,
        }}
      />
    </AbsoluteFill>
  );
};
