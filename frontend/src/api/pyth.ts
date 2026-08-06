// Live spot prices from oracle-service — the frontend's ONLY price source
// (SO-355).
//
// Opens a single WebSocket to oracle-service's price fanout (the same one
// every backend consumer reads) and dispatches frames to subscribers by
// feed id. Prices previously streamed straight from Pyth hermes-beta,
// which stopped publishing on 2026-08-04; oracle-service now sources them
// provider-appropriately (SO-353) and this module no longer knows or
// cares which oracle is live.
//
// Feed ids remain the catalog's `pythFeedId` — oracle-service publishes
// under those ids regardless of provider (they are cache keys, not a Pyth
// dependency), so both on-chain tickers (TBTC) and display aliases (BTC)
// resolve exactly as before. The one static alias left is `SUI` — native
// SUI is an ambient spot symbol, not a catalog token, so token-info
// serves no feed for it.

import { findToken, ORACLE_SERVICE_URL } from "../config";

export type PythPrice = {
  /** Hex feed id, lower-case, no `0x` prefix. */
  feedId: string;
  /** Ready for display (e.g. `64231.42`). */
  price: number;
  /** Confidence interval, same units as `price`. */
  conf: number;
  /** Publisher timestamp in unix seconds. */
  publishTime: number;
};

type Subscriber = (price: PythPrice) => void;

/**
 * Native SUI spot feed. SUI is an ambient display symbol used by spot widgets,
 * not a token-info catalog entry, so it has no served `pythFeedId` to consume.
 */
const SUI_FEED_ID = "0x50c67b3fd225db8912a424dd4baed60ffdde625ed2feaaf283724f9608fea266";

export function resolveFeedId(symbolOrId: string): string | null {
  const trimmed = symbolOrId.trim();
  if (/^0x[0-9a-f]{64}$/i.test(trimmed) || /^[0-9a-f]{64}$/i.test(trimmed)) {
    return normalize(trimmed);
  }
  // Supported tokens: resolve the feed served by the token-info catalog.
  const token = findToken(trimmed);
  if (token?.pythFeedId) {
    return normalize(token.pythFeedId);
  }
  // Ambient native SUI spot — not a catalog token.
  return trimmed.toUpperCase() === "SUI" ? normalize(SUI_FEED_ID) : null;
}

function normalize(id: string): string {
  return (id.startsWith("0x") ? id.slice(2) : id).toLowerCase();
}

/** `ws(s)://…/ws` derived from the service base URL. */
function wsUrl(): string {
  const base = ORACLE_SERVICE_URL.replace(/\/$/, "");
  return `${base.replace(/^http/, "ws")}/ws`;
}

// Wire frames, tagged `t` (oracle-client's `WsMessage`).
type WsFrame =
  | {
      t: "price";
      feed_id: string;
      price: number;
      conf: number;
      publish_time_ms: number;
    }
  | { t: "status"; upstream_healthy: boolean; reason?: string | null };

// `GET /prices` — one-shot seed so the UI paints before the first frame.
type PricesResponse = {
  prices?: Array<{
    feed_id: string;
    price: number;
    conf: number;
    publish_time_ms: number;
  }>;
};

/** Reconnect backoff bounds (jittered exponential). */
const BACKOFF_MIN_MS = 1_000;
const BACKOFF_MAX_MS = 30_000;

class OraclePriceClient {
  private subs = new Map<string, Set<Subscriber>>();
  private last = new Map<string, PythPrice>();
  private ws: WebSocket | null = null;
  private backoffMs = BACKOFF_MIN_MS;
  private retryTimer: ReturnType<typeof setTimeout> | null = null;
  private seeded = false;

  subscribe(feedId: string, cb: Subscriber): () => void {
    const id = normalize(feedId);
    let set = this.subs.get(id);
    if (!set) {
      set = new Set();
      this.subs.set(id, set);
    }
    set.add(cb);

    const cached = this.last.get(id);
    if (cached) cb(cached);
    this.connect();

    return () => {
      const s = this.subs.get(id);
      if (!s) return;
      s.delete(cb);
      if (s.size === 0) {
        this.subs.delete(id);
        // Keep `this.last` so a later re-subscribe (e.g. after navigation)
        // gets the cached value immediately.
        if (this.subs.size === 0) this.disconnect();
      }
    };
  }

  getLast(feedId: string): PythPrice | undefined {
    return this.last.get(normalize(feedId));
  }

  // The fanout broadcasts every feed on one socket, so unlike the old
  // per-feed Hermes stream the connection doesn't care WHICH feeds are
  // subscribed — only whether any are.
  private connect(): void {
    if (this.ws || this.retryTimer || this.subs.size === 0) return;

    if (!this.seeded) {
      this.seeded = true;
      void this.seedFromSnapshot();
    }

    let ws: WebSocket;
    try {
      ws = new WebSocket(wsUrl());
    } catch (e) {
      console.warn("[oracle] ws construction failed", e);
      this.scheduleReconnect();
      return;
    }
    this.ws = ws;

    ws.onopen = () => {
      this.backoffMs = BACKOFF_MIN_MS;
    };
    ws.onmessage = (ev) => {
      let frame: WsFrame;
      try {
        frame = JSON.parse(ev.data as string);
      } catch {
        return;
      }
      if (frame.t === "price") this.dispatch(frame);
      // `status` frames mark upstream connect-state; consumers judge
      // staleness by `publishTime`, so they're informational only.
    };
    ws.onclose = () => {
      this.ws = null;
      this.scheduleReconnect();
    };
    ws.onerror = () => {
      // onclose follows and owns the reconnect.
      console.warn("[oracle] price ws error; reconnecting");
    };
  }

  private disconnect(): void {
    if (this.retryTimer) {
      clearTimeout(this.retryTimer);
      this.retryTimer = null;
    }
    if (this.ws) {
      // Neuter the handler so this deliberate close doesn't schedule a
      // reconnect.
      this.ws.onclose = null;
      this.ws.close();
      this.ws = null;
    }
  }

  private scheduleReconnect(): void {
    if (this.retryTimer || this.subs.size === 0) return;
    const delay = this.backoffMs * (0.5 + Math.random());
    this.backoffMs = Math.min(this.backoffMs * 2, BACKOFF_MAX_MS);
    this.retryTimer = setTimeout(() => {
      this.retryTimer = null;
      this.connect();
    }, delay);
  }

  private dispatch(frame: Extract<WsFrame, { t: "price" }>): void {
    const id = normalize(frame.feed_id);
    const price: PythPrice = {
      feedId: id,
      price: frame.price,
      conf: frame.conf,
      publishTime: Math.floor(frame.publish_time_ms / 1000),
    };
    this.last.set(id, price);
    this.subs.get(id)?.forEach((cb) => cb(price));
  }

  /** Paint-on-load: seed the cache from the REST snapshot once. Live
   *  frames overwrite it as they arrive; failure is harmless. */
  private async seedFromSnapshot(): Promise<void> {
    try {
      const base = ORACLE_SERVICE_URL.replace(/\/$/, "");
      const res = await fetch(`${base}/prices`);
      if (!res.ok) return;
      const body = (await res.json()) as PricesResponse;
      for (const p of body.prices ?? []) {
        const id = normalize(p.feed_id);
        // Never clobber a fresher live frame with the snapshot.
        if (this.last.has(id)) continue;
        this.dispatch({ t: "price", ...p });
      }
    } catch {
      // Snapshot is best-effort; the stream is the source of truth.
    }
  }
}

export const pyth = new OraclePriceClient();
