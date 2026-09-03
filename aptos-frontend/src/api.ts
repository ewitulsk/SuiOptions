import { API_URL } from "./config";

export { octasToApt, venueName } from "./config";

export interface Listing {
  marketplace: string;
  listing_id: string;
  token_data_id: string;
  creator: string;
  collection: string;
  token_name: string;
  property_version: number | null;
  price: number;
  quote_token: string;
  seller: string;
  open_version: number;
}

export interface Status {
  indexer_cursor: number;
  live_listings: number;
  our_venue: string;
  router_package: string;
  router_config: string;
  venues: string[];
}

async function get<T>(path: string): Promise<T> {
  const r = await fetch(`${API_URL}${path}`);
  if (!r.ok) throw new Error(`GET ${path}: ${r.status}`);
  return r.json() as Promise<T>;
}

async function post<T>(path: string, body: unknown, token?: string): Promise<T> {
  const r = await fetch(`${API_URL}${path}`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      ...(token ? { authorization: `Bearer ${token}` } : {}),
    },
    body: JSON.stringify(body),
  });
  const data = await r.json();
  if (!r.ok) throw new Error((data as { error?: string }).error ?? `POST ${path}: ${r.status}`);
  return data as T;
}

export const api = {
  status: () => get<Status>("/status"),
  listings: (params: Record<string, string> = {}) => {
    const q = new URLSearchParams(params).toString();
    return get<Listing[]>(`/listings${q ? `?${q}` : ""}`);
  },
  item: (id: string) =>
    get<{ listing: Listing | null; activities: unknown[] }>(`/items/${id}`),
  txBuy: (body: { venue: number; standard: string; args: unknown[]; sender: string }) =>
    post<{ function: string; type_arguments: string[]; arguments: unknown[] }>("/tx/buy", body),
  txSweep: (body: {
    venues: number[];
    listings: string[];
    prices: string[];
    v1_creators: string[];
    v1_collections: string[];
    v1_names: string[];
    v1_property_versions: string[];
    sender: string;
  }) => post<{ function: string; type_arguments: string[]; arguments: unknown[] }>("/tx/sweep", body),
  adminVenues: (token: string, body: { config: string; venue: number; enable: boolean }) =>
    post("/admin/venues", body, token),
  adminFees: (token: string, body: { config: string; fee_bps: string; min_fee: string }) =>
    post("/admin/fees", body, token),
};
