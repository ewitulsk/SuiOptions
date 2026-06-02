import { AbsoluteFill } from "remotion";
import { Actor } from "../components/Actor";
import { Caption, Kicker } from "../components/Caption";
import { colors, roleColor } from "../theme";
import { POS } from "../layout";

export const CastScene: React.FC = () => {
  return (
    <AbsoluteFill>
      <Kicker index="01" title="The cast" />

      <Actor
        icon="🧑‍💻"
        title="Retail Writer"
        subtitle="Owns underlying · wants premium"
        accent={roleColor.writer}
        x={POS.writer[0]}
        y={POS.writer[1]}
        delay={4}
      />

      <Actor
        icon="📡"
        title="Quoting Service"
        subtitle="Routes RFQs · holds no funds"
        accent={roleColor.service}
        x={POS.service[0]}
        y={POS.service[1]}
        delay={12}
      />

      <Actor
        icon="🤖"
        title="Trader MM"
        subtitle="Buys calls · signs quotes"
        accent={roleColor.mm}
        x={POS.mm1[0]}
        y={POS.mm1[1]}
        width={280}
        delay={20}
      />
      <Actor
        icon="🤖"
        title="Trader MM"
        accent={roleColor.mm}
        x={POS.mm2[0]}
        y={POS.mm2[1]}
        width={280}
        delay={26}
      />
      <Actor
        icon="🤖"
        title="Trader MM"
        accent={roleColor.mm}
        x={POS.mm3[0]}
        y={POS.mm3[1]}
        width={280}
        delay={32}
      />

      <Actor
        icon="⛓️"
        title="Sui Contracts"
        subtitle="Bucket · Position · CallOption"
        accent={roleColor.chain}
        x={POS.chain[0]}
        y={POS.chain[1]}
        width={360}
        delay={40}
      />

      <Caption
        text="Four players. The writer and the market makers never talk directly — the Quoting Service brokers quotes, and the chain settles the trade."
        delay={48}
      />

      <div style={{ position: "absolute", top: 150, left: 80, color: colors.muted }} />
    </AbsoluteFill>
  );
};
