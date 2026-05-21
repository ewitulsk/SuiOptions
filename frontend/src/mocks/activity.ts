// Mock activity state — drop-in seam for indexer-backed event log + WSS tail.
import { useEffect, useMemo, useState } from "react";
import type {
  ActivityEvent,
  ActivityTotals,
  GroupedEvent,
} from "../types";

const ACTIVITY_SEED: ActivityEvent[] = [
  { id: "evt_t1", ts: "2026-05-16T17:02:14Z", type: "exercise",        side: "trader", status: "pending",   title: "Exercising 0.0500 BTC call · $85k", body: "Quote signed by argonaut_mm_01. Strike paid. Awaiting cursor advance.", value: { delta: 68.87, unit: "USDC" }, txHash: "0x4f81…b29a" },
  { id: "evt_t2", ts: "2026-05-16T16:48:02Z", type: "quote_received",  side: "trader", status: "info",      title: "3 fresh quotes from MM pool",       body: "argonaut_mm_01, phocas_capital, tidal_quants priced BTC·jun_26·$85k. Best: 91.20 USDC.", txHash: null },

  { id: "evt_y1", ts: "2026-05-15T22:14:08Z", type: "cursor_advance",  side: "writer", status: "confirmed", title: "Bucket cursor advanced · BTC·jun_26·$85k", body: "Cursor advanced 0.0420 → 0.860. 4% of your range was exercised against you.", value: { delta: 357.0, unit: "USDC" }, bucket: "BTC·jun_26·$85k", txHash: "0x9c2a…001f" },
  { id: "evt_y2", ts: "2026-05-15T19:33:21Z", type: "deposit",         side: "account", status: "confirmed", title: "Deposited 2,000 USDC to account", body: "from 0xc3f0…1144", value: { delta: 2000.0, unit: "USDC" }, txHash: "0xa1b4…3306" },

  { id: "evt_3d",   ts: "2026-05-13T11:08:55Z", type: "position_opened", side: "trader", status: "confirmed", title: "Bought 0.0200 BTC call · $94k", body: "Filled by tidal_quants in 96ms. Premium debited; call option tokens minted.", value: { delta: -26.10, unit: "USDC" }, bucket: "BTC·jun_26·$94k", txHash: "0x7e21…0a8c" },
  { id: "evt_3d_b", ts: "2026-05-13T11:08:48Z", type: "quote_signed",    side: "trader", status: "confirmed", title: "Signed quote · 26.10 USDC · ttl 24s", body: "Best ask from tidal_quants. Submitted to chain.", txHash: null },

  { id: "evt_4d",   ts: "2026-05-12T15:42:10Z", type: "position_opened", side: "writer", status: "confirmed", title: "Wrote 200 SUI call · $4.20", body: "Filled by phocas_capital in 188ms. Premium credited; position NFT minted (range [1820, 2020)).", value: { delta: 18.60, unit: "USDC" }, bucket: "SUI·jun_26·$4.20", txHash: "0xc8e0…02ab" },

  { id: "evt_5d",   ts: "2026-05-11T10:14:33Z", type: "position_opened", side: "trader", status: "confirmed", title: "Bought 400 SUI call · $4.50", body: "Filled by phocas_capital. Premium debited; call option tokens minted.", value: { delta: -28.40, unit: "USDC" }, bucket: "SUI·jun_26·$4.50", txHash: "0xc8e0…02ab" },

  { id: "evt_8d",   ts: "2026-05-08T13:55:00Z", type: "position_opened", side: "trader", status: "confirmed", title: "Bought 0.0500 BTC call · $85k", body: "Filled by argonaut_mm_01 in 142ms. Premium debited; call option tokens minted.", value: { delta: -91.38, unit: "USDC" }, bucket: "BTC·jun_26·$85k", txHash: "0x9a3f…0042" },
  { id: "evt_8d_b", ts: "2026-05-08T13:54:51Z", type: "quote_signed",    side: "trader", status: "confirmed", title: "Signed quote · 91.38 USDC · ttl 28s", body: "Best ask from argonaut_mm_01. Submitted to chain.", txHash: null },

  { id: "evt_11d",  ts: "2026-05-05T08:22:11Z", type: "quote_signed",    side: "trader", status: "expired",   title: "Quote expired before submission", body: "phocas_capital quote on BTC·jun_26·$84k expired in your wallet (ttl 30s). No funds moved.", txHash: null },

  { id: "evt_12d",  ts: "2026-05-04T19:08:42Z", type: "position_opened", side: "writer", status: "confirmed", title: "Wrote 0.0500 BTC call · $85k", body: "Filled by argonaut_mm_01. Premium credited; position NFT minted (range [0.840, 0.890)).", value: { delta: 86.72, unit: "USDC" }, bucket: "BTC·jun_26·$85k", txHash: "0x16dd…00a4" },

  { id: "evt_25d",   ts: "2026-04-24T20:00:01Z", type: "claim",          side: "writer", status: "confirmed", title: "Claimed settlement · BTC·apr_24·$80k", body: "Range [0.200, 0.300) settled. 0.035 BTC exercised → received 2,800 USDC; 0.065 BTC returned as collateral.", value: { delta: 2800.0, unit: "USDC" }, bucket: "BTC·apr_24·$80k", txHash: "0x33f2…0bba" },
  { id: "evt_25d_b", ts: "2026-04-23T11:52:00Z", type: "cursor_advance", side: "writer", status: "confirmed", title: "Bucket cursor reached expiry · BTC·apr_24·$80k", body: "Final cursor 0.235. 35% of your range was exercised against you.", bucket: "BTC·apr_24·$80k", txHash: "0x21cc…0099" },

  { id: "evt_60d",   ts: "2026-03-18T14:00:00Z", type: "position_opened", side: "writer",  status: "confirmed", title: "Wrote 0.1000 BTC call · $80k (apr_24)", body: "Filled by tidal_quants. Premium credited; position NFT minted (range [0.200, 0.300)).", value: { delta: 142.40, unit: "USDC" }, bucket: "BTC·apr_24·$80k", txHash: "0x42b1…0177" },
  { id: "evt_60d_b", ts: "2026-03-15T09:00:00Z", type: "deposit",         side: "account", status: "confirmed", title: "Deposited 0.5 BTC to account", body: "from 0xc3f0…1144", value: { delta: 0.5, unit: "BTC" }, txHash: "0x0844…0010" },
  { id: "evt_60d_c", ts: "2026-03-15T08:55:43Z", type: "deposit",         side: "account", status: "confirmed", title: "Deposited 5,000 USDC to account", body: "from 0xc3f0…1144", value: { delta: 5000.0, unit: "USDC" }, txHash: "0x0844…0007" },
  { id: "evt_60d_d", ts: "2026-03-15T08:50:00Z", type: "connect",         side: "account", status: "info",      title: "Wallet connected · tideline.fi", body: "0x9f3a…42b1 · sui mainnet", txHash: null },
];

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
  withdraw:        { label: "Withdraw",         icon: "↑", accent: "mute" },
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

export function useActivityState(): ActivityState {
  const [events] = useState<ActivityEvent[]>(ACTIVITY_SEED);
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
