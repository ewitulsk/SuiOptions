import { useQuery } from "@tanstack/react-query";
import { Link, useParams, useSearchParams } from "react-router-dom";
import { api, octasToApt, venueName } from "../api";

export default function Collection() {
  const { id } = useParams();
  const [search] = useSearchParams();
  const venue = search.get("venue") ?? undefined;
  const { data } = useQuery({
    queryKey: ["collection", id, venue],
    queryFn: () =>
      api.listings({
        ...(id ? { collection: id } : {}),
        ...(venue ? { marketplace: venue } : {}),
        limit: "100",
      }),
  });
  return (
    <div>
      <h2>{id ?? "All collections"}</h2>
      <ul>
        {(data ?? []).map((l) => (
          <li key={`${l.marketplace}:${l.listing_id}`}>
            <Link to={`/items/${l.token_data_id || l.listing_id}`}>
              {l.token_name}
            </Link>{" "}
            — {octasToApt(l.price)} APT · {venueName(l.marketplace)}
          </li>
        ))}
      </ul>
    </div>
  );
}
