// Lightweight Pyth price client for the browser.
//
// Opens a single Hermes SSE stream covering every feed id that has at least
// one live subscriber. Re-runs the connection whenever the active set
// changes. Mirrors the transport used by `rust-backend/crates/pyth-client`.
//
// Supported-token feed ids come from the token-info catalog
// (`SUPPORTED_TOKENS[].pythFeedId`). A few bare market symbols (BTC/SUI/USDC),
// used by ambient spot widgets that work in real-asset terms, keep a small
// static alias map — they are display-only and not part of the catalog.

import { SUPPORTED_TOKENS } from "../config";

const HERMES_BASE = "https://hermes.pyth.network";

export type PythPrice = {
  /** Hex feed id, lower-case, no `0x` prefix. */
  feedId: string;
  /** Scaled by `expo` — ready for display (e.g. `64231.42`). */
  price: number;
  /** Confidence interval, scaled the same way as `price`. */
  conf: number;
  /** Publisher timestamp in unix seconds. */
  publishTime: number;
};

type Subscriber = (price: PythPrice) => void;

/**
 * Bare market-symbol aliases for ambient spot widgets (e.g. the dashboard's
 * BTC/SUI tickers). These are NOT supported-token catalog entries — supported
 * tokens resolve their feed from `SUPPORTED_TOKENS[].pythFeedId` instead.
 */
const DISPLAY_ALIASES: Record<string, string> = {
  BTC: "0xe62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43",
  USDC: "0xeaa020c61cc479712813461ce153894a96a6c00b21ed0cfc2798d1f9a9e9c94a",
  SUI: "0x23d7315113f5b1d3ba7a83604c44b94d79f4fd69af77f804fc7f920a6dc65744",
};

export function resolveFeedId(symbolOrId: string): string | null {
  const trimmed = symbolOrId.trim();
  if (/^0x[0-9a-f]{64}$/i.test(trimmed) || /^[0-9a-f]{64}$/i.test(trimmed)) {
    return normalize(trimmed);
  }
  const upper = trimmed.toUpperCase();
  // Supported tokens: resolve the feed from the token-info catalog.
  const token = SUPPORTED_TOKENS.find((t) => t.ticker.toUpperCase() === upper);
  if (token?.pythFeedId) {
    return normalize(token.pythFeedId);
  }
  // Ambient display symbols (BTC/SUI/USDC).
  const alias = DISPLAY_ALIASES[upper];
  return alias ? normalize(alias) : null;
}

function normalize(id: string): string {
  return (id.startsWith("0x") ? id.slice(2) : id).toLowerCase();
}

type ParsedUpdate = {
  id: string;
  price: { price: string; conf: string; expo: number; publish_time: number };
};

class PythClient {
  private subs = new Map<string, Set<Subscriber>>();
  private last = new Map<string, PythPrice>();
  private es: EventSource | null = null;
  private restartPending = false;
  // Sorted, comma-joined feed ids currently bound to `this.es`. Used to skip
  // a tear-down when subscribe/unsubscribe churn (e.g. route changes) leaves
  // the active set unchanged.
  private streamKey: string | null = null;

  subscribe(feedId: string, cb: Subscriber): () => void {
    const id = normalize(feedId);
    let set = this.subs.get(id);
    if (!set) {
      set = new Set();
      this.subs.set(id, set);
      this.scheduleRestart();
    }
    set.add(cb);

    const cached = this.last.get(id);
    if (cached) cb(cached);

    return () => {
      const s = this.subs.get(id);
      if (!s) return;
      s.delete(cb);
      if (s.size === 0) {
        this.subs.delete(id);
        // Keep `this.last` so a later re-subscribe (e.g. after navigation)
        // gets the cached value immediately while the stream reopens.
        this.scheduleRestart();
      }
    };
  }

  getLast(feedId: string): PythPrice | undefined {
    return this.last.get(normalize(feedId));
  }

  // Coalesce multiple subscribe/unsubscribe calls in the same tick into one
  // stream restart.
  private scheduleRestart(): void {
    if (this.restartPending) return;
    this.restartPending = true;
    queueMicrotask(() => {
      this.restartPending = false;
      this.restart();
    });
  }

  private restart(): void {
    const ids = [...this.subs.keys()].sort();
    const key = ids.join(",");
    if (key === this.streamKey && this.es) return;

    if (this.es) {
      this.es.close();
      this.es = null;
    }
    this.streamKey = key || null;
    if (ids.length === 0) return;

    const qs = ids.map((id) => `ids[]=${id}`).join("&");
    const es = new EventSource(`${HERMES_BASE}/v2/updates/price/stream?${qs}`);

    es.onmessage = (ev) => {
      let data: { parsed?: ParsedUpdate[] };
      try {
        data = JSON.parse(ev.data);
      } catch {
        return;
      }
      const parsed = data.parsed;
      if (!parsed) return;
      for (const item of parsed) {
        const id = normalize(item.id);
        const scale = Math.pow(10, item.price.expo);
        const price: PythPrice = {
          feedId: id,
          price: Number(item.price.price) * scale,
          conf: Number(item.price.conf) * scale,
          publishTime: item.price.publish_time,
        };
        this.last.set(id, price);
        this.subs.get(id)?.forEach((cb) => cb(price));
      }
    };

    // EventSource reconnects on its own; just log so dropped streams are
    // visible during development.
    es.onerror = () => {
      console.warn("[pyth] hermes stream error; browser will reconnect");
    };

    this.es = es;
  }
}

export const pyth = new PythClient();
