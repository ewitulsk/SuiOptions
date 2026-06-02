import { AbsoluteFill, useCurrentFrame, useVideoConfig } from "remotion";
import { colors, fonts } from "../theme";
import { rise, fade } from "../anim";

export const TitleScene: React.FC = () => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const e = rise(frame, fps, 6);
  const sub = fade(frame, 24, 48);
  const tag = fade(frame, 40, 64);

  return (
    <AbsoluteFill
      style={{
        justifyContent: "center",
        alignItems: "center",
        fontFamily: fonts.sans,
        textAlign: "center",
      }}
    >
      <div
        style={{
          opacity: tag,
          color: colors.sui,
          fontFamily: fonts.mono,
          letterSpacing: 6,
          fontSize: 24,
          marginBottom: 24,
        }}
      >
        COVERED-CALL OPTIONS · BUILT ON SUI
      </div>
      <div
        style={{
          transform: `scale(${0.9 + e * 0.1})`,
          opacity: e,
          fontSize: 96,
          fontWeight: 800,
          color: colors.text,
          letterSpacing: -1,
        }}
      >
        SuiOptions
      </div>
      <div
        style={{
          opacity: sub,
          marginTop: 18,
          fontSize: 40,
          fontWeight: 600,
          color: colors.teal,
        }}
      >
        The Write Flow — RFQ &amp; Fill
      </div>
      <div
        style={{
          opacity: tag,
          marginTop: 40,
          fontSize: 24,
          color: colors.muted,
          maxWidth: 900,
          lineHeight: 1.5,
        }}
      >
        How a retail writer sells a covered call to a market maker — from quote
        request to on-chain settlement.
      </div>
    </AbsoluteFill>
  );
};
