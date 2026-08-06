// Shared "desk not available" banner: disabled ([desk] off — the prod
// shape), unreachable, or still booting.

import type { UseQueryResult } from "@tanstack/react-query";

import type { DeskStateResponse } from "../api/deskState";
import { useDashEnv } from "../config";

export function DeskDownBanner(props: { query: UseQueryResult<DeskStateResponse> }) {
  const env = useDashEnv();
  const { query } = props;
  if (query.isLoading) {
    return <div className="dash-banner dash-banner--info">Loading desk state…</div>;
  }
  if (query.isError) {
    const msg = query.error instanceof Error ? query.error.message : String(query.error);
    return (
      <div className="dash-banner">
        mm-bot unreachable on <b>{env}</b>: {msg}. The desk may be restarting — this page
        retries automatically.
      </div>
    );
  }
  return (
    <div className="dash-banner dash-banner--info">
      The desk is <b>disabled</b> on <b>{env}</b> (<code>[desk] enabled = false</code>) — the
      bot serves health/auth only and declines every RFQ. Switch environment (top right) to
      view an active desk.
    </div>
  );
}
