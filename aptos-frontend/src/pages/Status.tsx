import { useQuery } from "@tanstack/react-query";
import { api } from "../api";

export default function Status() {
  const { data } = useQuery({
    queryKey: ["status"],
    queryFn: api.status,
    refetchInterval: 15000,
  });
  if (!data) return <p>Loading…</p>;
  return (
    <div>
      <h2>Pipeline status</h2>
      <ul>
        <li>Indexer cursor: {data.indexer_cursor}</li>
        <li>Live listings: {data.live_listings}</li>
        <li>
          Our venue: <code>{data.our_venue || "(not deployed)"}</code>
        </li>
        <li>
          Router: <code>{data.router_package || "(not deployed)"}</code>
        </li>
        <li>Venues: {data.venues.join(", ")}</li>
      </ul>
    </div>
  );
}
