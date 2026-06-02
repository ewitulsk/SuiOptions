import { AbsoluteFill, Series } from "remotion";
import { Background } from "./components/Background";
import { TitleScene } from "./scenes/TitleScene";
import { CastScene } from "./scenes/CastScene";
import { RfqScene } from "./scenes/RfqScene";
import { QuotesScene } from "./scenes/QuotesScene";
import { FillScene } from "./scenes/FillScene";
import { CursorScene } from "./scenes/CursorScene";
import { OutroScene } from "./scenes/OutroScene";

// Scene durations (frames @ 30fps). Sum drives TOTAL_FRAMES in Root.tsx.
export const SCENES = [
  { c: TitleScene, d: 120 },
  { c: CastScene, d: 165 },
  { c: RfqScene, d: 240 },
  { c: QuotesScene, d: 270 },
  { c: FillScene, d: 300 },
  { c: CursorScene, d: 300 },
  { c: OutroScene, d: 180 },
];

export const TOTAL_FRAMES = SCENES.reduce((n, s) => n + s.d, 0);

export const ExplainerVideo: React.FC = () => {
  return (
    <AbsoluteFill>
      <Background />
      <Series>
        {SCENES.map((s, i) => (
          <Series.Sequence key={i} durationInFrames={s.d}>
            <s.c />
          </Series.Sequence>
        ))}
      </Series>
    </AbsoluteFill>
  );
};
