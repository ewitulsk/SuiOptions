// Singleton WebSocket client for the quoting service.
//
// Lazily connects on first `sendRfq`, performs the Hello/HelloAck handshake,
// correlates RFQResponse frames back to the originating call via `request_id`,
// reflects Ping → Pong, and reconnects with exponential backoff on drop.
//
// The Rust spec lives at `rust-backend/crates/protocol-types/src/messages.rs`
// — keep this file's frame shapes (in `./quoting.ts`) in sync.

import type {
  BucketSpec,
  BulkViewPremium,
  RetailRole,
  RetailToService,
  RfqQuoteEntry,
  ServiceToRetail,
  Side,
} from "./quoting";

const QUOTING_WS_URL: string =
  (import.meta.env.VITE_QUOTING_WS_URL as string | undefined) ??
  "ws://127.0.0.1:9002/";

export type QuotingStatus = "connecting" | "connected" | "disconnected";

const CLIENT_VERSION = "0.0.1";
const RECONNECT_BASE_MS = 500;
const RECONNECT_MAX_MS = 8_000;
const RFQ_TIMEOUT_MS = 6_000;

type Pending = {
  resolve: (entries: RfqQuoteEntry[]) => void;
  reject: (err: Error) => void;
  timer: ReturnType<typeof setTimeout>;
};

type PendingBulk = {
  resolve: (premiums: BulkViewPremium[]) => void;
  reject: (err: Error) => void;
  timer: ReturnType<typeof setTimeout>;
};

class QuotingClient {
  private ws: WebSocket | null = null;
  /** In-flight connect attempt; subsequent callers await the same promise. */
  private connecting: Promise<void> | null = null;
  private helloAcked = false;
  /** Sent in the Hello payload — informational on the server side, doesn't change RFQ routing. */
  private role: RetailRole = "writer";
  private pending = new Map<string, Pending>();
  private pendingBulk = new Map<string, PendingBulk>();
  private reconnectAttempt = 0;
  private explicitClose = false;
  private status: QuotingStatus = "disconnected";
  private listeners = new Set<() => void>();

  /** Current connection status — for the useSyncExternalStore snapshot. */
  getStatus = (): QuotingStatus => this.status;

  /** Subscribe to status changes. Returns an unsubscribe fn. */
  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn);
    return () => {
      this.listeners.delete(fn);
    };
  };

  /** Open the socket proactively (e.g. to surface live connection status). */
  connect_(): Promise<void> {
    return this.ensureConnected();
  }

  private setStatus(s: QuotingStatus) {
    if (this.status === s) return;
    this.status = s;
    for (const fn of this.listeners) fn();
  }

  async sendRfq(args: {
    spec: BucketSpec;
    writeAmount: string;
    side: Side;
    requestId?: string;
  }): Promise<RfqQuoteEntry[]> {
    await this.ensureConnected();
    const ws = this.ws;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      throw new Error("quoting ws not open after connect");
    }
    const requestId = args.requestId ?? crypto.randomUUID();
    const frame: RetailToService = {
      type: "RFQRequest",
      request_id: requestId,
      payload: {
        spec: args.spec,
        write_amount: args.writeAmount,
        side: args.side,
      },
    };
    return new Promise<RfqQuoteEntry[]>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(requestId);
        reject(new Error(`RFQ ${requestId} timed out after ${RFQ_TIMEOUT_MS}ms`));
      }, RFQ_TIMEOUT_MS);
      this.pending.set(requestId, { resolve, reject, timer });
      try {
        ws.send(JSON.stringify(frame));
      } catch (e) {
        clearTimeout(timer);
        this.pending.delete(requestId);
        reject(e instanceof Error ? e : new Error(String(e)));
      }
    });
  }

  /**
   * Request indicative (unsigned) premiums for many buckets at one write
   * amount — drives the tile display. Resolves with one entry per bucket the
   * service had a value for (omitting buckets no MM priced). Never reserves
   * MM balance or produces a signable quote.
   */
  async sendBulkView(args: {
    specs: BucketSpec[];
    writeAmount: string;
    side: Side;
    requestId?: string;
  }): Promise<BulkViewPremium[]> {
    await this.ensureConnected();
    const ws = this.ws;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      throw new Error("quoting ws not open after connect");
    }
    const requestId = args.requestId ?? crypto.randomUUID();
    const frame: RetailToService = {
      type: "BulkViewRFQRequest",
      request_id: requestId,
      payload: {
        specs: args.specs,
        write_amount: args.writeAmount,
        side: args.side,
      },
    };
    return new Promise<BulkViewPremium[]>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pendingBulk.delete(requestId);
        reject(
          new Error(`bulk-view ${requestId} timed out after ${RFQ_TIMEOUT_MS}ms`),
        );
      }, RFQ_TIMEOUT_MS);
      this.pendingBulk.set(requestId, { resolve, reject, timer });
      try {
        ws.send(JSON.stringify(frame));
      } catch (e) {
        clearTimeout(timer);
        this.pendingBulk.delete(requestId);
        reject(e instanceof Error ? e : new Error(String(e)));
      }
    });
  }

  private async ensureConnected(): Promise<void> {
    if (
      this.ws &&
      this.ws.readyState === WebSocket.OPEN &&
      this.helloAcked
    ) {
      return;
    }
    if (!this.connecting) {
      this.connecting = this.connect();
    }
    await this.connecting;
  }

  private connect(): Promise<void> {
    this.explicitClose = false;
    this.setStatus("connecting");
    return new Promise<void>((resolve, reject) => {
      const ws = new WebSocket(QUOTING_WS_URL);
      this.ws = ws;
      this.helloAcked = false;
      let settled = false;

      ws.addEventListener("open", () => {
        const hello: RetailToService = {
          type: "Hello",
          payload: { role: this.role, version: CLIENT_VERSION },
        };
        try {
          ws.send(JSON.stringify(hello));
        } catch (e) {
          settled = true;
          reject(e instanceof Error ? e : new Error(String(e)));
        }
      });

      ws.addEventListener("message", (ev) => {
        let msg: ServiceToRetail;
        try {
          msg = JSON.parse(String(ev.data));
        } catch {
          return;
        }
        if (msg.type === "HelloAck") {
          this.helloAcked = true;
          this.reconnectAttempt = 0;
          this.connecting = null;
          this.setStatus("connected");
          settled = true;
          resolve();
          return;
        }
        this.dispatch(msg);
      });

      const onClose = () => {
        if (this.ws === ws) this.ws = null;
        const wasAcked = this.helloAcked;
        this.helloAcked = false;
        this.connecting = null;
        this.setStatus("disconnected");
        for (const [id, p] of this.pending) {
          clearTimeout(p.timer);
          p.reject(
            new Error(`quoting ws closed before response (request ${id})`),
          );
        }
        this.pending.clear();
        for (const [, p] of this.pendingBulk) {
          clearTimeout(p.timer);
          // Display-only; resolve empty so the tiles keep their placeholder.
          p.resolve([]);
        }
        this.pendingBulk.clear();
        if (!settled) {
          settled = true;
          reject(new Error("quoting ws closed before HelloAck"));
        }
        if (!this.explicitClose) {
          this.scheduleReconnect();
        }
        if (!wasAcked) {
          // Surface to the console so a missing/unreachable quoting
          // service doesn't fail silently in dev.
          console.warn(
            "[quoting] disconnected before HelloAck — is the service running on",
            QUOTING_WS_URL,
            "?",
          );
        }
      };

      ws.addEventListener("close", onClose);
      ws.addEventListener("error", () => {
        // The browser fires `error` then `close`; let `close` handle teardown.
      });
    });
  }

  private scheduleReconnect() {
    const delay = Math.min(
      RECONNECT_MAX_MS,
      RECONNECT_BASE_MS * 2 ** this.reconnectAttempt,
    );
    this.reconnectAttempt += 1;
    setTimeout(() => {
      if (this.ws == null) {
        // Best-effort warm reconnect. The next `sendRfq` will await
        // `ensureConnected` if this attempt fails again.
        this.connecting = this.connect().catch(() => undefined) as Promise<void>;
      }
    }, delay);
  }

  private dispatch(msg: ServiceToRetail) {
    if (msg.type === "RFQResponse") {
      const p = this.pending.get(msg.request_id);
      if (!p) return;
      clearTimeout(p.timer);
      this.pending.delete(msg.request_id);
      p.resolve(msg.payload.quotes);
      return;
    }
    if (msg.type === "BulkViewRFQResponse") {
      const p = this.pendingBulk.get(msg.request_id);
      if (!p) return;
      clearTimeout(p.timer);
      this.pendingBulk.delete(msg.request_id);
      p.resolve(msg.payload.premiums);
      return;
    }
    if (msg.type === "Error") {
      const rid = msg.request_id ?? null;
      if (rid) {
        const p = this.pending.get(rid);
        if (p) {
          clearTimeout(p.timer);
          this.pending.delete(rid);
          // `no_quotes` is the common case — no MMs answered in the
          // window. Treat as "zero quotes" rather than a thrown error so
          // the UI can render the empty state.
          if (msg.payload.code === "no_quotes") {
            p.resolve([]);
          } else {
            p.reject(
              new Error(`${msg.payload.code}: ${msg.payload.message}`),
            );
          }
          return;
        }
        const pb = this.pendingBulk.get(rid);
        if (pb) {
          clearTimeout(pb.timer);
          this.pendingBulk.delete(rid);
          // Bulk view is best-effort display; surface an error as "no
          // premiums" so tiles fall back to the placeholder rather than throw.
          pb.resolve([]);
          return;
        }
      }
      console.warn("[quoting] error", msg.payload);
      return;
    }
    if (msg.type === "Ping") {
      const pong: RetailToService = { type: "Pong" };
      try {
        this.ws?.send(JSON.stringify(pong));
      } catch {
        /* ignore — close handler will tear down */
      }
      return;
    }
    // BucketUpdate is currently ignored — the api-service /buckets
    // endpoint is the source of truth for bucket data.
  }
}

export const quotingClient = new QuotingClient();
