import { IndexerProgressBar } from "tideline-frontend";

// Indexer checkpoint-ingestion progress. Fetches GET /indexer/progress via
// react-query (useIndexerProgress) — no props. With no api-service in preview
// the fetch fails and the component shows its "indexer status unavailable"
// floor card; with a live backend it shows the filled bar + checkpoint/rate/
// eta stats and a "live" pill once caught up.

export const Status = () => <IndexerProgressBar />;
