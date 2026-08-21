// Public leaderboard read API (go-backend `leaderboard` service).
//
// Same base-URL convention as the other clients: defaults to local dev,
// override with `VITE_LEADERBOARD_URL` (see config.ts). Four read-only
// endpoints:
//   GET /leaderboard — one ranked page for a (window, source) filter
//   GET /rank/{wallet} — the wallet's rank plus its ranked neighbors
//   GET /account/{wallet}/breakdown — per-source points for one account
//   GET /sources — the source catalog feeding the filter dropdown
//
// Errors: 4xx `{"error":"..."}` for bad params; 503 when the service is
// unreachable/unhealthy — surfaced as `LeaderboardUnavailableError` so the
// UI can show a friendly "temporarily unavailable" state. A 404 from
// /rank or /breakdown means "wallet unknown / no points in window" and
// resolves to `null` rather than an error.

import { keepPreviousData, useQuery } from "@tanstack/react-query";

import { LEADERBOARD_URL } from "../config";

const base = LEADERBOARD_URL.replace(/\/$/, "");

export type LeaderboardWindow = "all" | "30d" | "7d" | "24h";

/** One ranked row, as served inside /leaderboard and /rank neighbors. */
export type LeaderboardEntry = {
  rank: number;
  account_id: number;
  /** Normalized wallet addresses linked to the account (may be empty for
   * twitter-only accounts). */
  wallets: string[];
  twitter: string | null;
  points: number;
  event_count: number;
};

export type LeaderboardPage = {
  window: LeaderboardWindow;
  source: string;
  as_of_ms: number;
  total_accounts: number;
  limit: number;
  offset: number;
  entries: LeaderboardEntry[];
};

export type LeaderboardRank = {
  rank: number;
  points: number;
  account_id: number;
  wallets: string[];
  twitter: string | null;
  /** Ranked rows around (and including) the target account. */
  neighbors: LeaderboardEntry[];
  total_accounts: number;
};

export type BreakdownSource = {
  source: string;
  label: string | null;
  event_type: string | null;
  points: number;
  event_count: number;
  last_event_ms: number | null;
};

export type LeaderboardBreakdown = {
  account_id: number;
  total: number;
  by_source: BreakdownSource[];
};

export type LeaderboardSource = {
  source: string;
  label: string | null;
  event_type: string | null;
};

export type LeaderboardSources = { sources: LeaderboardSource[] };

/** 503 from the leaderboard service — treat as "temporarily unavailable". */
export class LeaderboardUnavailableError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "LeaderboardUnavailableError";
  }
}

/** Non-503 error response, carrying the HTTP status. */
export class LeaderboardRequestError extends Error {
  readonly status: number;
  constructor(message: string, status: number) {
    super(message);
    this.name = "LeaderboardRequestError";
    this.status = status;
  }
}

async function fetchLeaderboard<T>(path: string): Promise<T> {
  const res = await fetch(`${base}${path}`);
  if (!res.ok) {
    let msg = `${res.status} ${res.statusText}`;
    try {
      const body = (await res.json()) as { error?: string };
      if (body?.error) msg = body.error;
    } catch {
      // non-JSON error body; keep the status text
    }
    if (res.status === 503) throw new LeaderboardUnavailableError(msg);
    throw new LeaderboardRequestError(`GET ${path.split("?")[0]} failed: ${msg}`, res.status);
  }
  return (await res.json()) as T;
}

export type LeaderboardParams = {
  window: LeaderboardWindow;
  /** `""` = all sources. */
  source: string;
  limit: number;
  offset: number;
};

export function useLeaderboard(p: LeaderboardParams) {
  return useQuery<LeaderboardPage, Error>({
    queryKey: ["leaderboard", p.window, p.source, p.limit, p.offset],
    queryFn: () => {
      const qs = new URLSearchParams({
        window: p.window,
        source: p.source,
        limit: String(p.limit),
        offset: String(p.offset),
      });
      return fetchLeaderboard<LeaderboardPage>(`/leaderboard?${qs}`);
    },
    // Keep the previous page rendered while the next one loads so paging
    // and filter changes don't flash the table away.
    placeholderData: keepPreviousData,
    staleTime: 30_000,
    retry: 1,
  });
}

export function useLeaderboardSources() {
  return useQuery<LeaderboardSources, Error>({
    queryKey: ["leaderboard", "sources"],
    queryFn: () => fetchLeaderboard<LeaderboardSources>("/sources"),
    staleTime: 30_000,
    retry: 1,
  });
}

/** Rank + neighbors for one wallet; `null` when the wallet is unknown or
 * has no points in the window (the service 404s). */
export function useLeaderboardRank(address: string | null, window: LeaderboardWindow) {
  return useQuery<LeaderboardRank | null, Error>({
    queryKey: ["leaderboard", "rank", address, window],
    queryFn: async () => {
      const qs = new URLSearchParams({ window });
      try {
        return await fetchLeaderboard<LeaderboardRank>(
          `/rank/${encodeURIComponent(address!)}?${qs}`,
        );
      } catch (e) {
        if (e instanceof LeaderboardRequestError && e.status === 404) return null;
        throw e;
      }
    },
    enabled: address !== null,
    staleTime: 30_000,
    retry: 1,
  });
}

/** Per-source breakdown for one wallet's account; `null` on 404 (unknown
 * wallet). Fetched lazily — callers mount the hook only when a row expands. */
export function useLeaderboardBreakdown(wallet: string | null, window: LeaderboardWindow) {
  return useQuery<LeaderboardBreakdown | null, Error>({
    queryKey: ["leaderboard", "breakdown", wallet, window],
    queryFn: async () => {
      const qs = new URLSearchParams({ window });
      try {
        return await fetchLeaderboard<LeaderboardBreakdown>(
          `/account/${encodeURIComponent(wallet!)}/breakdown?${qs}`,
        );
      } catch (e) {
        if (e instanceof LeaderboardRequestError && e.status === 404) return null;
        throw e;
      }
    },
    enabled: wallet !== null,
    staleTime: 30_000,
    retry: 1,
  });
}
