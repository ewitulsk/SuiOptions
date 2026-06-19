// Live activity state — indexer-backed event log via api-service `/events`,
// polled for a near-live tail. Drop-in replacement for `mocks/activity.ts`:
// returns the same `ActivityState` shape and re-exports the same
// helpers/meta/filters so `Activity.tsx` only changes its import path.
//
// A WSS event tail is a follow-up; polling (10s) covers the acceptance
// "new events appear without a manual refresh" until then.
import { useEffect, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";

import { fetchActivity, type EventDto } from "../api/client";
import type {
  ActivityEvent,
  ActivityTotals,
  EventStatus,
  GroupedEvent,
  Side,
} from "../types";

export type EventMeta = { label: string; icon: string; accent: "info" | "pos" | "warm" | "mute" | "neg" };

export const EVENT_TYPE_META: Record<string, EventMeta> = {
  exercise:        { label: "Exercise",         icon: "X", accent: "warm" },
  position_opened: { label: "Position opened",  icon: "+", accent: "info" },
  cursor_advance:  { label: "Cursor advance",   icon: "→", accent: "info" },
  quote_signed:    { label: "Quote signed",     icon: "✎", accent: "mute" },
  quote_received:  { label: "Quote received",   icon: "·", accent: "mute" },
  claim:           { label: "Settlement claim", icon: "✓", accent: "pos" },
  close_early:     { label: "Closed early",     icon: "←", accent: "mute" },
  deposit:         { label: "Deposit",          icon: "↓", accent: "pos" },
  withdraw:        { label: "Withdraw",          icon: "↑", accent: "mute" },
  connect:         { label: "Connected",        icon: "·", accent: "mute" },
};

export type ActivityFilter =
  | "all" | "trader" | "writer" | "exercise" | "claims" | "account";

export const ACTIVITY_FILTERS: { key: ActivityFilter; label: string }[] = [
  { key: "all",      label: "All activity"     },
  { key: "trader",   label: "Trades"           },
  { key: "writer",   label: "Writes"           },
  { key: "exercise", label: "Exercises"        },
  { key: "claims",   label: "Claims & cursor"  },
  { key: "account",  label: "Account"          },
];

export type ActivityState = {
  events: ActivityEvent[];
  filtered: ActivityEvent[];
  grouped: GroupedEvent[];
  filter: ActivityFilter;
  setFilter: (f: ActivityFilter) => void;
  now: number;
  totals: ActivityTotals;
};

// ── DTO → ActivityEvent ─────────────────────────────────────────────────

function fmtAmount(n: number): string {
  return n.toLocaleString("en-US", { maximumFractionDigits: 4 });
}

function fmtStrike(strike: number | null): string {
  if (strike === null) return "";
  if (strike >= 1000) {
    const k = strike / 1000;
    return `$${k.toLocaleString("en-US", { maximumFractionDigits: 1 })}k`;
  }
  return `$${strike.toLocaleString("en-US", { maximumFractionDigits: 2 })}`;
}

function bucketLabel(e: EventDto): string | undefined {
  if (!e.asset_symbol || e.strike === null) return undefined;
  const date = e.expiry_ms
    ? new Date(e.expiry_ms).toLocaleDateString("en-US", { month: "short", day: "numeric" })
    : "";
  return [e.asset_symbol, date, fmtStrike(e.strike)].filter(Boolean).join("·");
}

/** Compose the human title/body the timeline renders, from structured DTO fields. */
function titleAndBody(e: EventDto): { title: string; body: string } {
  const asset = e.asset_symbol ?? "";
  const amt = e.amount !== null ? fmtAmount(e.amount) : "";
  const strike = fmtStrike(e.strike);
  const unit = e.value_unit ?? "";
  const absVal = e.value_delta !== null ? fmtAmount(Math.abs(e.value_delta)) : "";
  switch (e.type) {
    case "position_opened":
      return e.side === "writer"
        ? { title: `Wrote ${amt} ${asset} call · ${strike}`, body: "Premium credited; position Object minted." }
        : { title: `Bought ${amt} ${asset} call · ${strike}`, body: "Premium debited; call option tokens minted." };
    case "exercise":
      return { title: `Exercised ${amt} ${asset} call · ${strike}`, body: "Strike paid; underlying received." };
    case "claim":
      return { title: `Claimed settlement · ${bucketLabel(e) ?? asset}`, body: "Position closed; settlement + collateral returned." };
    case "deposit":
      return { title: `Deposited ${absVal} ${unit} to account`, body: "Account balance credited." };
    case "withdraw":
      return { title: `Withdrew ${absVal} ${unit} from account`, body: "Account balance debited." };
    default:
      return { title: e.type, body: "" };
  }
}

function toActivityEvent(e: EventDto): ActivityEvent {
  const { title, body } = titleAndBody(e);
  return {
    id: e.id,
    ts: e.ts_iso,
    type: e.type,
    side: e.side as Side,
    status: e.status as EventStatus,
    title,
    body,
    value:
      e.value_delta !== null
        ? { delta: e.value_delta, unit: e.value_unit ?? "" }
        : undefined,
    txHash: e.tx_hash,
    bucket: bucketLabel(e),
  };
}

// ── the hook ──────────────────────────────────────────────────────────

export function useActivityState(wallet: string | null): ActivityState {
  const query = useQuery<EventDto[], Error>({
    queryKey: ["events", wallet],
    enabled: wallet !== null,
    refetchInterval: 10_000,
    queryFn: () => fetchActivity(wallet as string),
  });

  const events = useMemo<ActivityEvent[]>(
    () => (query.data ?? []).map(toActivityEvent),
    [query.data],
  );

  const [filter, setFilter] = useState<ActivityFilter>("all");
  const [now, setNow] = useState(Date.now());

  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 30000);
    return () => clearInterval(t);
  }, []);

  const filtered = useMemo(
    () =>
      events.filter((e) => {
        if (filter === "all") return true;
        if (filter === "trader")   return e.side === "trader" || e.type === "exercise";
        if (filter === "writer")   return e.side === "writer";
        if (filter === "exercise") return e.type === "exercise";
        if (filter === "claims")   return e.type === "claim" || e.type === "cursor_advance";
        if (filter === "account")  return e.side === "account";
        return true;
      }),
    [events, filter],
  );

  const grouped = useMemo<GroupedEvent[]>(() => {
    const out: GroupedEvent[] = [];
    let last: string | null = null;
    for (const e of filtered) {
      const d = new Date(e.ts);
      const dayKey = d.toISOString().slice(0, 10);
      if (last !== dayKey) {
        out.push({ kind: "day", key: dayKey, date: d });
        last = dayKey;
      }
      out.push({ kind: "event", e });
    }
    return out;
  }, [filtered]);

  const totals = useMemo<ActivityTotals>(() => {
    let exercises = 0;
    let writes = 0;
    let buys = 0;
    let deposits = 0;
    let premiumIn = 0;
    let premiumOut = 0;
    for (const e of events) {
      if (e.type === "exercise") exercises++;
      if (e.type === "position_opened" && e.side === "writer") {
        writes++;
        if (e.value) premiumIn += e.value.delta;
      }
      if (e.type === "position_opened" && e.side === "trader") {
        buys++;
        if (e.value) premiumOut += Math.abs(e.value.delta);
      }
      if (e.type === "deposit") deposits++;
    }
    return { exercises, writes, buys, deposits, premiumIn, premiumOut };
  }, [events]);

  return { events, filtered, grouped, filter, setFilter, now, totals };
}

export function relativeTime(ts: string, now: number): string {
  const diff = (now - new Date(ts).getTime()) / 1000;
  if (diff < 60) return "just now";
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
  if (diff < 604800) return `${Math.floor(diff / 86400)}d ago`;
  return new Date(ts).toLocaleDateString("en-US", { month: "short", day: "numeric" });
}

export function formatDay(date: Date): string {
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  const d = new Date(date);
  d.setHours(0, 0, 0, 0);
  const diff = (today.getTime() - d.getTime()) / 86400000;
  if (diff === 0) return "Today";
  if (diff === 1) return "Yesterday";
  return date.toLocaleDateString("en-US", {
    weekday: "long",
    month: "short",
    day: "numeric",
  });
}
