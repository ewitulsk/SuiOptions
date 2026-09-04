import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import { api, octasToApt, venueName } from "../api";
import { VENUES } from "../config";

export default function Landing() {
  const { data, isLoading, error } = useQuery({
    queryKey: ["listings"],
    queryFn: () => api.listings({ limit: "50" }),
  });
  return (
    <div>
      <h1>NFT Marketplace</h1>
      <p>Every live listing on every tracked Aptos venue, one sweep away.</p>
      <div style={{ display: "flex", gap: 8, flexWrap: "wrap", marginBottom: 12 }}>
        {VENUES.map((v) => (
          <Link key={v.slug} to={`/?venue=${v.slug}`}>
            {v.name}
          </Link>
        ))}
      </div>
      {isLoading && <p>Loading…</p>}
      {error && <p>Backend unreachable — is the API up?</p>}
      <ul>
        {(data ?? []).map((l) => (
          <li key={`${l.marketplace}:${l.listing_id}`}>
            <Link to={`/items/${l.token_data_id || l.listing_id}`}>
              {l.token_name || l.listing_id.slice(0, 12)}…
            </Link>{" "}
            — {octasToApt(l.price)} APT · {venueName(l.marketplace)} ·{" "}
            {l.collection}
          </li>
        ))}
      </ul>
    </div>
  );
}
