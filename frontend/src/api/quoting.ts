// WebSocket client for the quoting service (§5.4).
//
// Implements the retail ↔ service protocol: Hello handshake, RFQ requests,
// and response/error handling. The connection auto-reconnects on drop.
//
// Wire format mirrors `protocol-types/src/messages.rs`:
//   { "type": "<variant>", "request_id"?: "...", "payload": { ... } }

// ---------------------------------------------------------------------------
// env
// ---------------------------------------------------------------------------

const QUOTING_WS_URL: string =
  (import.meta.env.VITE_QUOTING_WS_URL as string | undefined) ??
  "ws://127.0.0.1:9002";

// ---------------------------------------------------------------------------
// wire types (retail ↔ service subset)
// ---------------------------------------------------------------------------

export type RfqRequestPayload = {
  bucket_id: string;
  write_amount: string; // u64 decimal-string
  side: "writer" | "trader";
};

export type RfqQuoteEntry = {
  quote: {
    protocol_id: string;
    signer_account_id: string;
    signer_token_recipient: string;
    bucket_id: string;
    write_amount: string;
    premium: string;
    valid_until_ms: string;
    nonce: string;
  };
  signature: string;
  mm_id: string;
  mm_reputation: number;
};

export type RfqResponsePayload = {
  bucket_id: string;
  write_amount: string;
  quotes: RfqQuoteEntry[];
};

export type ErrorPayload = {
  code: string;
  message: string;
};

export type ServiceToRetailMsg =
  | { type: "HelloAck"; payload: { session_id: string } }
  | { type: "BucketUpdate"; payload: { bucket_id: string; total_written: string; exercise_cursor: string; expiry_ms: string } }
  | { type: "RFQResponse"; request_id: string; payload: RfqResponsePayload }
  | { type: "Error"; request_id: string | null; payload: ErrorPayload }
  | { type: "Ping" };

// ---------------------------------------------------------------------------
// event callbacks
// ---------------------------------------------------------------------------

export type QuotingEvents = {
  onOpen?: () => void;
  onClose?: () => void;
  onHelloAck?: (sessionId: string) => void;
  onRfqResponse?: (requestId: string, payload: RfqResponsePayload) => void;
  onError?: (requestId: string | null, payload: ErrorPayload) => void;
  onRawMessage?: (msg: ServiceToRetailMsg) => void;
};

// ---------------------------------------------------------------------------
// client
// ---------------------------------------------------------------------------

let idCounter = 0;

export function nextRequestId(): string {
  return `rfq-${Date.now()}-${++idCounter}`;
}

export class QuotingClient {
  private ws: WebSocket | null = null;
  private events: QuotingEvents;
  private url: string;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private disposed = false;
  private _connected = false;
  private _sessionId: string | null = null;

  constructor(events: QuotingEvents, url?: string) {
    this.events = events;
    this.url = url ?? QUOTING_WS_URL;
  }

  get connected(): boolean {
    return this._connected;
  }

  get sessionId(): string | null {
    return this._sessionId;
  }

  connect(): void {
    if (this.ws) return;
    this.disposed = false;

    const wsUrl = this.url.replace(/^http/, "ws");
    const ws = new WebSocket(wsUrl);

    ws.onopen = () => {
      // Send Hello immediately.
      ws.send(
        JSON.stringify({
          type: "Hello",
          payload: { role: "writer", version: "1" },
        }),
      );
    };

    ws.onmessage = (ev) => {
      let msg: ServiceToRetailMsg;
      try {
        msg = JSON.parse(ev.data as string);
      } catch {
        return;
      }
      this.events.onRawMessage?.(msg);

      switch (msg.type) {
        case "HelloAck":
          this._connected = true;
          this._sessionId = msg.payload.session_id;
          this.events.onHelloAck?.(msg.payload.session_id);
          this.events.onOpen?.();
          break;
        case "RFQResponse":
          this.events.onRfqResponse?.(msg.request_id, msg.payload);
          break;
        case "Error":
          this.events.onError?.(msg.request_id, msg.payload);
          break;
        case "Ping":
          ws.send(JSON.stringify({ type: "Pong" }));
          break;
        default:
          break;
      }
    };

    ws.onclose = () => {
      this._connected = false;
      this._sessionId = null;
      this.ws = null;
      this.events.onClose?.();
      this.scheduleReconnect();
    };

    ws.onerror = () => {
      // onerror is always followed by onclose; nothing extra needed.
    };

    this.ws = ws;
  }

  disconnect(): void {
    this.disposed = true;
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.ws) {
      this.ws.close();
      this.ws = null;
    }
    this._connected = false;
    this._sessionId = null;
  }

  sendRfq(payload: RfqRequestPayload, requestId?: string): string {
    const id = requestId ?? nextRequestId();
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      console.warn("[quoting] sendRfq while disconnected, request_id:", id);
      return id;
    }
    this.ws.send(
      JSON.stringify({
        type: "RFQRequest",
        request_id: id,
        payload,
      }),
    );
    return id;
  }

  private scheduleReconnect(): void {
    if (this.disposed || this.reconnectTimer) return;
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      if (!this.disposed) this.connect();
    }, 3000);
  }
}
