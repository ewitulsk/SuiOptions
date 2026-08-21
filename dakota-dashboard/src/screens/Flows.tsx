import { useQuery } from "@tanstack/react-query";

import * as api from "../api/dakota";
import { EventFeed, FlowsTable, TotalsPanel } from "../components/ActivityTable";
import { ErrorBox } from "../components/ui";
import { useAuthed } from "../state/session";

/** Activity and amount flows.
 *
 *  Identical for every role — the service decides what "everything" means from
 *  the token, so an admin sees the platform, a business sees its roster and an
 *  individual sees itself, all from the same two calls. */
export default function Flows({ title = "Flows" }: { title?: string }) {
  const { token } = useAuthed();
  const flows = useQuery({ queryKey: ["flows"], queryFn: () => api.getFlows(token) });
  const feed = useQuery({ queryKey: ["feed"], queryFn: () => api.getFeed(token) });

  return (
    <>
      <h2>{title}</h2>
      <ErrorBox error={flows.error ?? feed.error} />
      <TotalsPanel totals={flows.data?.totals ?? []} />
      <FlowsTable flows={flows.data?.by_customer ?? []} />
      <EventFeed events={feed.data ?? []} />
    </>
  );
}
