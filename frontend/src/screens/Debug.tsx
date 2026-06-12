import { IndexerProgressBar } from "../components/IndexerProgressBar";
import { LiveBuckets } from "../components/LiveBuckets";
import { useQuotingStatus } from "../api/useQuotingStatus";

export function Debug() {
  const quotingStatus = useQuotingStatus();
  return (
    <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
      <div className="app__wrap">
        <div className="dash-hero">
          <div className="dash-hero__eyebrow">internals</div>
          <h1 className="dash-hero__title">Debug</h1>
          <div className="dash-hero__addr">indexer ingestion status</div>
          <span className={"conn-status conn-status--" + quotingStatus}>
            <span className="conn-status__dot" />
            quoting service: {quotingStatus}
          </span>
        </div>

        <IndexerProgressBar />
        <LiveBuckets />
      </div>
    </div>
  );
}
