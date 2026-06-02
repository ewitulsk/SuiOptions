import { AbsoluteFill, useCurrentFrame, useVideoConfig } from "remotion";
import { Actor } from "../components/Actor";
import { Caption, Kicker } from "../components/Caption";
import { MessagePacket } from "../components/MessagePacket";
import { Wires } from "../components/Wire";
import { colors, fonts, roleColor } from "../theme";
import { POS } from "../layout";
import { rise, fade } from "../anim";

const ValidationCard: React.FC<{ delay: number }> = ({ delay }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const e = rise(frame, fps, delay);
  const checks = ["Ed25519 signature", "Quote not expired", "MM has balance", "Reserve premium"];
  return (
    <div
      style={{
        position: "absolute",
        left: POS.service[0],
        top: POS.service[1] + 130,
        transform: "translate(-50%, 0)",
        opacity: e,
        width: 340,
        background: colors.panel,
        border: `1px solid ${roleColor.service}`,
        borderRadius: 16,
        padding: "16px 22px",
        fontFamily: fonts.mono,
      }}
    >
      <div style={{ color: roleColor.service, fontSize: 18, marginBottom: 10 }}>
        SERVICE VALIDATES
      </div>
      {checks.map((c, i) => {
        const on = fade(frame, delay + 10 + i * 8, delay + 18 + i * 8);
        return (
          <div key={c} style={{ display: "flex", gap: 10, fontSize: 19, padding: "4px 0" }}>
            <span style={{ color: colors.green, opacity: on }}>✓</span>
            <span style={{ color: colors.text, opacity: 0.5 + on * 0.5 }}>{c}</span>
          </div>
        );
      })}
    </div>
  );
};

const Leaderboard: React.FC<{ delay: number }> = ({ delay }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const e = rise(frame, fps, delay);
  const quotes = [
    { mm: "MM-2", prem: "$92.40", best: true },
    { mm: "MM-1", prem: "$90.10", best: false },
    { mm: "MM-3", prem: "$88.75", best: false },
  ];
  return (
    <div
      style={{
        position: "absolute",
        left: POS.writer[0],
        top: POS.writer[1] + 150,
        transform: `translate(-50%, 0) scale(${0.9 + e * 0.1})`,
        opacity: e,
        width: 340,
        background: colors.panel,
        border: `1px solid ${colors.stroke}`,
        borderRadius: 16,
        padding: "16px 20px",
        fontFamily: fonts.mono,
      }}
    >
      <div style={{ color: colors.teal, fontSize: 18, marginBottom: 10 }}>
        RFQResponse · sorted by premium
      </div>
      {quotes.map((q, i) => {
        const on = fade(frame, delay + 8 + i * 6, delay + 16 + i * 6);
        return (
          <div
            key={q.mm}
            style={{
              display: "flex",
              justifyContent: "space-between",
              alignItems: "center",
              opacity: on,
              background: q.best ? `${colors.teal}22` : "transparent",
              border: q.best ? `1px solid ${colors.teal}` : "1px solid transparent",
              borderRadius: 10,
              padding: "8px 12px",
              margin: "4px 0",
            }}
          >
            <span style={{ color: colors.muted, fontSize: 19 }}>{q.mm}</span>
            <span style={{ color: q.best ? colors.teal : colors.text, fontSize: 22, fontWeight: 700 }}>
              {q.prem} {q.best ? "★" : ""}
            </span>
          </div>
        );
      })}
    </div>
  );
};

export const QuotesScene: React.FC = () => {
  return (
    <AbsoluteFill>
      <Kicker index="03" title="Quotes &amp; reservation" />

      <Wires
        lines={[
          { from: POS.mm1, to: POS.service, color: roleColor.mm, start: 0, duration: 16 },
          { from: POS.mm2, to: POS.service, color: roleColor.mm, start: 0, duration: 16 },
          { from: POS.mm3, to: POS.service, color: roleColor.mm, start: 0, duration: 16 },
          { from: POS.service, to: POS.writer, color: roleColor.service, start: 120, duration: 16 },
        ]}
      />

      <Actor icon="🧑‍💻" title="Retail Writer" accent={roleColor.writer} x={POS.writer[0]} y={POS.writer[1]} width={300} />
      <Actor icon="📡" title="Quoting Service" accent={roleColor.service} x={POS.service[0]} y={POS.service[1]} />
      <Actor icon="🤖" title="Trader MM" accent={roleColor.mm} x={POS.mm1[0]} y={POS.mm1[1]} width={260} />
      <Actor icon="🤖" title="Trader MM" accent={roleColor.mm} x={POS.mm2[0]} y={POS.mm2[1]} width={260} />
      <Actor icon="🤖" title="Trader MM" accent={roleColor.mm} x={POS.mm3[0]} y={POS.mm3[1]} width={260} />

      <MessagePacket from={POS.mm1} to={POS.service} label="Quote $90.10" sub="signed" color={roleColor.mm} start={8} duration={26} />
      <MessagePacket from={POS.mm2} to={POS.service} label="Quote $92.40" sub="signed" color={roleColor.mm} start={14} duration={26} />
      <MessagePacket from={POS.mm3} to={POS.service} label="Quote $88.75" sub="signed" color={roleColor.mm} start={20} duration={26} />

      <ValidationCard delay={48} />

      <MessagePacket
        from={POS.service}
        to={POS.writer}
        label="RFQResponse"
        sub="3 quotes · best first"
        color={roleColor.service}
        start={122}
        duration={30}
      />

      <Leaderboard delay={160} />

      <Caption
        text="Each MM signs a quote with their hot key. The service checks the signature, confirms the MM can back it, reserves the premium, and returns the quotes — best price first."
        delay={170}
      />
    </AbsoluteFill>
  );
};
