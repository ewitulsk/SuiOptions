import { AbsoluteFill, useCurrentFrame } from "remotion";
import { Actor } from "../components/Actor";
import { Caption, Kicker } from "../components/Caption";
import { MessagePacket } from "../components/MessagePacket";
import { Wires } from "../components/Wire";
import { colors, fonts, roleColor } from "../theme";
import { POS } from "../layout";
import { rise } from "../anim";
import { useVideoConfig } from "remotion";

const BucketCard: React.FC<{ delay: number }> = ({ delay }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const e = rise(frame, fps, delay);
  return (
    <div
      style={{
        position: "absolute",
        left: POS.writer[0],
        top: POS.writer[1] + 150,
        transform: `translate(-50%, 0) scale(${0.9 + e * 0.1})`,
        opacity: e,
        width: 320,
        background: colors.panel,
        border: `1px solid ${colors.stroke}`,
        borderRadius: 16,
        padding: "18px 22px",
        fontFamily: fonts.mono,
      }}
    >
      <div style={{ color: colors.teal, fontSize: 18, marginBottom: 10 }}>
        SELECTED BUCKET
      </div>
      <Row k="asset" v="wBTC" />
      <Row k="strike" v="$70,000" />
      <Row k="expiry" v="28 JUN" />
      <Row k="write_amount" v="0.10 wBTC" />
    </div>
  );
};

const Row: React.FC<{ k: string; v: string }> = ({ k, v }) => (
  <div
    style={{
      display: "flex",
      justifyContent: "space-between",
      fontSize: 19,
      padding: "3px 0",
    }}
  >
    <span style={{ color: colors.muted }}>{k}</span>
    <span style={{ color: colors.text, fontWeight: 700 }}>{v}</span>
  </div>
);

export const RfqScene: React.FC = () => {
  return (
    <AbsoluteFill>
      <Kicker index="02" title="Request for quote" />

      <Wires
        lines={[
          { from: POS.writer, to: POS.service, color: roleColor.service, start: 20, duration: 18 },
          { from: POS.service, to: POS.mm1, color: roleColor.mm, start: 70, duration: 16 },
          { from: POS.service, to: POS.mm2, color: roleColor.mm, start: 70, duration: 16 },
          { from: POS.service, to: POS.mm3, color: roleColor.mm, start: 70, duration: 16 },
        ]}
      />

      <Actor icon="🧑‍💻" title="Retail Writer" accent={roleColor.writer} x={POS.writer[0]} y={POS.writer[1]} width={300} />
      <Actor icon="📡" title="Quoting Service" accent={roleColor.service} x={POS.service[0]} y={POS.service[1]} />
      <Actor icon="🤖" title="Trader MM" accent={roleColor.mm} x={POS.mm1[0]} y={POS.mm1[1]} width={260} />
      <Actor icon="🤖" title="Trader MM" accent={roleColor.mm} x={POS.mm2[0]} y={POS.mm2[1]} width={260} />
      <Actor icon="🤖" title="Trader MM" accent={roleColor.mm} x={POS.mm3[0]} y={POS.mm3[1]} width={260} />

      <BucketCard delay={4} />

      <MessagePacket
        from={POS.writer}
        to={POS.service}
        label="RFQRequest"
        sub="bucket · 0.10 wBTC · side=writer"
        color={roleColor.writer}
        start={30}
        duration={28}
      />

      <MessagePacket from={POS.service} to={POS.mm1} label="RFQBroadcast" color={roleColor.service} start={78} duration={26} />
      <MessagePacket from={POS.service} to={POS.mm2} label="RFQBroadcast" color={roleColor.service} start={84} duration={26} />
      <MessagePacket from={POS.service} to={POS.mm3} label="RFQBroadcast" color={roleColor.service} start={90} duration={26} />

      <Caption
        text="The writer asks for a price. The Quoting Service fans the request out to every connected Trader MM, each given a 2-second window to respond."
        delay={120}
      />
    </AbsoluteFill>
  );
};
