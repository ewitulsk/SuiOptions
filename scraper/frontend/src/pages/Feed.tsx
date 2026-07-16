import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { ApiError, Listing, Valuation, get, post } from "../api";

function margin(listing: Listing, v: Valuation): number {
  return (Number(v.est_resale_low) + Number(v.est_resale_high)) / 2 - Number(listing.price);
}

function ListingCard({ listing }: { listing: Listing }) {
  const queryClient = useQueryClient();
  const valuation = listing.valuations[listing.valuations.length - 1];

  const track = useMutation({
    mutationFn: () => post("/api/deals", { listing_id: listing.id }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["deals"] }),
  });

  return (
    <div className="card">
      <div className="row">
        <a href={listing.url} target="_blank" rel="noreferrer">
          <strong>{listing.title}</strong>
        </a>
        <span className="badge">{listing.source}</span>
        {listing.location && <span className="muted">{listing.location}</span>}
      </div>
      <div className="row" style={{ marginTop: 8 }}>
        <span>
          Asking <strong>${listing.price}</strong>
        </span>
        {valuation && (
          <>
            <span>
              Est. resale{" "}
              <strong>
                ${valuation.est_resale_low}–${valuation.est_resale_high}
              </strong>
            </span>
            <span className={margin(listing, valuation) > 0 ? "green" : "red"}>
              ~${margin(listing, valuation).toFixed(0)} margin
            </span>
            <span className="muted">
              max buy ${valuation.max_buy_price} · {(valuation.confidence * 100).toFixed(0)}%
              confidence
              {valuation.expected_days_to_sell != null &&
                ` · ~${valuation.expected_days_to_sell}d to sell`}
            </span>
          </>
        )}
      </div>
      {listing.photos.length > 0 && (
        <div className="photo-strip" style={{ marginTop: 10 }}>
          {listing.photos.slice(0, 5).map((url) => (
            <img key={url} src={url} alt="" />
          ))}
        </div>
      )}
      {valuation?.rationale && (
        <p className="muted" style={{ marginBottom: 4 }}>
          {valuation.rationale}
        </p>
      )}
      {valuation && valuation.risk_flags.length > 0 && (
        <div className="muted">⚠ {valuation.risk_flags.join(", ")}</div>
      )}
      <div className="row" style={{ marginTop: 10 }}>
        <button onClick={() => track.mutate()} disabled={track.isPending || track.isSuccess}>
          {track.isSuccess ? "Tracking" : "Track as deal"}
        </button>
        {valuation?.outreach_draft && (
          <button
            className="secondary"
            onClick={() => navigator.clipboard.writeText(valuation.outreach_draft!)}
          >
            Copy outreach draft
          </button>
        )}
        {track.isError && (
          <span className="error">
            {track.error instanceof ApiError ? track.error.message : "Failed"}
          </span>
        )}
      </div>
    </div>
  );
}

export default function Feed() {
  const listings = useQuery<Listing[]>({
    queryKey: ["listings", "valued"],
    queryFn: () => get<Listing[]>("/api/listings?valued_only=true"),
    refetchInterval: 30_000,
  });

  if (listings.isLoading) return <p className="muted">Loading…</p>;
  if (listings.isError) return <p className="error">Failed to load listings.</p>;

  return (
    <>
      <h1>Deal feed</h1>
      {listings.data!.length === 0 && (
        <p className="muted">
          Nothing valued yet. Add a saved search and the worker will start pulling listings.
        </p>
      )}
      {listings.data!.map((l) => (
        <ListingCard key={l.id} listing={l} />
      ))}
    </>
  );
}
