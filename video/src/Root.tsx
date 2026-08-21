import { Composition } from "remotion";
import { ExplainerVideo, TOTAL_FRAMES } from "./ExplainerVideo";

export const RemotionRoot: React.FC = () => {
  return (
    <Composition
      id="WriteFlowExplainer"
      component={ExplainerVideo}
      durationInFrames={TOTAL_FRAMES}
      fps={30}
      width={1920}
      height={1080}
    />
  );
};
