import { Header } from "../components/Header";
import { WaveHero } from "../components/WaveHero";
import { IndexerProgressBar } from "../components/IndexerProgressBar";
import { LiveBuckets } from "../components/LiveBuckets";

export function Debug() {
  return (
    <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
      <WaveHero />
      <Header />

      <div className="app__wrap">
        <div className="dash-hero">
          <div className="dash-hero__eyebrow">internals</div>
          <h1 className="dash-hero__title">Debug</h1>
          <div className="dash-hero__addr">indexer ingestion status</div>
        </div>

        <IndexerProgressBar />
        <LiveBuckets />
      </div>
    </div>
  );
}
