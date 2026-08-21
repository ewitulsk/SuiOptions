import { AbsoluteFill, useCurrentFrame, useVideoConfig } from "remotion";
import { Actor } from "../components/Actor";
import { Caption, Kicker } from "../components/Caption";
import { MessagePacket } from "../components/MessagePacket";
import { colors, fonts, roleColor } from "../theme";
import { rise, fade } from "../anim";

// Local layout: writer left, the contract centre-stage, MM + treasury right.
const L = {
  writer: [320, 480] as [number, number],
  chain: [960, 470] as [number, number],
  mm: [1620, 330] as [number, number],
  treasury: [1620, 620] as [number, number],
};

const STEPS = [
  "verify_and_consume_quote()  — sig · nonce · expiry",
  "skim fee → Treasury",
  "route net premium → Writer",
  "move underlying → Bucket",
  "assign cursor range [start, end)",
  "mint Position → Writer · CallOption → MM",
];

const ExecutePanel: React.FC<{ delay: number }> = ({ delay }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const e = rise(frame, fps, delay);
  return (
    <div
      style={{
        position: "absolute",
        left: L.chain[0],
        top: L.chain[1],
        transform: `translate(-50%, -50%) scale(${0.92 + e * 0.08})`,
        opacity: e,
        width: 560,
        background: `linear-gradient(180deg, ${colors.panelLight}, ${colors.panel})`,
        border: `2px solid ${roleColor.chain}`,
        borderRadius: 20,
        padding: "24px 28px",
        fontFamily: fonts.mono,
        boxShadow: "0 24px 70px #000a",
      }}
    >
      <div style={{ color: roleColor.chain, fontSize: 22, fontWeight: 700, marginBottom: 16 }}>
        execute_write&lt;wBTC, USDC&gt;()
      </div>
      {STEPS.map((s, i) => {
        const on = fade(frame, delay + 16 + i * 14, delay + 28 + i * 14);
        return (
          <div
            key={s}
            style={{
              display: "flex",
              gap: 12,
              alignItems: "center",
              fontSize: 19,
              padding: "7px 0",
              opacity: 0.35 + on * 0.65,
            }}
          >
            <span
              style={{
                color: colors.bg,
                background: on > 0.5 ? colors.green : colors.muted,
                borderRadius: 6,
                width: 24,
                height: 24,
                display: "flex",
                alignItems: "center",
                justifyContent: "center",
                fontWeight: 800,
                fontSize: 15,
              }}
            >
              {on > 0.5 ? "✓" : i + 1}
            </span>
            <span style={{ color: colors.text }}>{s}</span>
          </div>
        );
      })}
    </div>
  );
};

const EventBanner: React.FC<{ delay: number }> = ({ delay }) => {
  const frame = useCurrentFrame();
  const e = fade(frame, delay, delay + 14);
  return (
    <div
      style={{
        position: "absolute",
        left: L.chain[0],
        top: L.chain[1] + 250,
        transform: "translate(-50%, 0)",
        opacity: e,
        fontFamily: fonts.mono,
        color: roleColor.chain,
        border: `1px dashed ${roleColor.chain}`,
        borderRadius: 10,
        padding: "8px 18px",
        fontSize: 20,
      }}
    >
      📣 emit WriteExecuted
    </div>
  );
};

export const FillScene: React.FC = () => {
  return (
    <AbsoluteFill>
      <Kicker index="04" title="The fill · execute_write" />

      <Actor icon="🧑‍💻" title="Retail Writer" accent={roleColor.writer} x={L.writer[0]} y={L.writer[1]} width={280} />
      <Actor icon="🤖" title="Trader MM (MM-2)" accent={roleColor.mm} x={L.mm[0]} y={L.mm[1]} width={300} />
      <Actor icon="🏦" title="Treasury" subtitle="protocol fee" accent={colors.green} x={L.treasury[0]} y={L.treasury[1]} width={300} />

      <ExecutePanel delay={6} />

      {/* Writer submits the signed PTB to the chain. */}
      <MessagePacket
        from={L.writer}
        to={L.chain}
        label="PTB: execute_write"
        sub="underlying + chosen SignedQuote"
        color={roleColor.writer}
        start={6}
        duration={30}
      />

      {/* Outputs distributed once the steps complete. */}
      <MessagePacket from={L.chain} to={L.writer} label="net premium $92.40" color={colors.teal} start={150} duration={28} />
      <MessagePacket from={L.chain} to={L.writer} label="Position NFT" sub="range [start, end)" color={colors.teal} start={168} duration={28} />
      <MessagePacket from={L.chain} to={L.mm} label="CallOption" sub="amount = 0.10" color={roleColor.mm} start={158} duration={28} />
      <MessagePacket from={L.chain} to={L.treasury} label="fee" color={colors.green} start={150} duration={28} />

      <EventBanner delay={196} />

      <Caption
        text="The writer signs one transaction. On-chain, the contract verifies the MM's quote, skims the fee, pays the writer their premium, and mints a Position NFT to the writer and a CallOption to the MM — atomically."
        delay={210}
      />
    </AbsoluteFill>
  );
};
