// Thin typed fetch wrapper. Session cookie rides along automatically
// (same-origin in prod via Caddy, proxied in dev via vite.config.ts).

export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const resp = await fetch(path, {
    credentials: "same-origin",
    headers: init?.body ? { "Content-Type": "application/json" } : undefined,
    ...init,
  });
  if (!resp.ok) {
    let detail = resp.statusText;
    try {
      const body = await resp.json();
      if (typeof body.detail === "string") detail = body.detail;
    } catch {
      /* keep statusText */
    }
    throw new ApiError(resp.status, detail);
  }
  if (resp.status === 204) return undefined as T;
  return resp.json();
}

export const get = <T>(path: string) => request<T>(path);
export const post = <T>(path: string, body?: unknown) =>
  request<T>(path, { method: "POST", body: body ? JSON.stringify(body) : undefined });
export const patch = <T>(path: string, body: unknown) =>
  request<T>(path, { method: "PATCH", body: JSON.stringify(body) });
export const put = <T>(path: string, body: unknown) =>
  request<T>(path, { method: "PUT", body: JSON.stringify(body) });
export const del = (path: string) => request<void>(path, { method: "DELETE" });

// ---- types mirroring the backend response models ----

export interface User {
  id: number;
  username: string;
}

export interface SavedSearch {
  id: number;
  source: string;
  name: string;
  query: string;
  category: string | null;
  min_price: string | null;
  max_price: string | null;
  poll_interval_seconds: number;
  alert_threshold: number;
  active: boolean;
  last_polled_at: string | null;
  created_at: string;
}

export interface Valuation {
  id: number;
  model: string;
  est_resale_low: string;
  est_resale_high: string;
  expected_days_to_sell: number | null;
  max_buy_price: string;
  confidence: number;
  risk_flags: string[];
  resale_channel: string | null;
  rationale: string | null;
  outreach_draft: string | null;
  created_at: string;
}

export interface Listing {
  id: number;
  source: string;
  external_id: string;
  url: string;
  title: string;
  description: string | null;
  price: string;
  currency: string;
  location: string | null;
  photos: string[];
  seller: string | null;
  posted_at: string | null;
  scraped_at: string;
  saved_search_id: number | null;
  triage_passed: boolean | null;
  triage_reason: string | null;
  valuations: Valuation[];
}

export interface Deal {
  id: number;
  listing_id: number | null;
  title: string;
  status: string;
  buy_price: string | null;
  buy_extra_costs: string;
  bought_at: string | null;
  bought_by: number | null;
  sale_price: string | null;
  sale_fees: string;
  sold_at: string | null;
  sale_channel: string | null;
  notes: string | null;
  net_profit: string | null;
  created_at: string;
  updated_at: string;
}

export interface DealStats {
  realized_profit_all_time: string;
  realized_profit_30d: string;
  capital_tied_up: string;
  deals_sold: number;
  win_rate: number;
  avg_days_to_sell: number | null;
  per_user: { user_id: number; username: string; deals_bought: number; realized_profit: string }[];
}
