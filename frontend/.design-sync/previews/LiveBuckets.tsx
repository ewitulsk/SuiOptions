import { LiveBuckets } from "tideline-frontend";

// Live view of on-chain bucket series. Fetches GET /buckets via react-query
// (useBuckets) — no props. With no api-service in preview the fetch fails and
// the section shows its title plus an error/empty status line; with a live
// backend each series renders as a collapsible <details> with a strike/written/
// exercised/fill/bucket-id table.

export const Buckets = () => <LiveBuckets />;
