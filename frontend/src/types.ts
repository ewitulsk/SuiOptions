// Shared domain types. UI components consume these — mocks in `src/mocks/`
// produce them, but a real Sui SDK / indexer layer can produce the same shapes.

export type Side = "writer" | "trader" | "account";
export type View = "writer" | "trader";

export type Strike = {
  strike: number;
  perUnit: number;
  premium: number;
  premiumDisplay: string;
};

export type Quote = {
  id: string;
  name: string;
  addr: string;
  fill: number;
  revertRate: number;
  latency: number;
  premium: number;
  ttl: number;
  arrivedAt: number;
};

export type Bucket = {
  cursor: number;
  queued: number;
  cap: number;
};

export type ConfirmStage = "signing" | "broadcast" | "confirmed" | null;

export type ConfirmSummary = {
  view: View;
  premium: number;
  bucket: string;
  rangeStart: number;
  rangeEnd: number;
  amount: number;
  strike: number;
  asset: string;
  expiry: string;
};

// Dashboard

export type OwnedPosition = {
  id: string;
  side: "owned";
  asset: "BTC" | "SUI" | string;
  strike: number;
  expiry: string;
  amount: number;
  premiumPaid: number;
  boughtFrom: string;
  boughtAt: string;
  rangeId: string;
  // decorated
  spot: number;
  dte: number;
  itm: boolean;
  moneyness: number;
  intrinsicNow: number;
  pnl: number;
  status:
    | "exercisable"
    | "active_otm"
    | "expired_itm"
    | "expired_otm";
};

export type WrittenPosition = {
  id: string;
  side: "written";
  asset: "BTC" | "SUI" | string;
  strike: number;
  expiry: string;
  amount: number;
  premiumReceived: number;
  soldTo: string;
  soldAt: string;
  rangeStart: number;
  rangeEnd: number;
  cursorAtSale?: number;
  cursorAtExpiry?: number;
  // decorated
  spot: number;
  dte: number;
  exercisedQty: number;
  totalQty: number;
  exercisedPct: number;
  cursor: number;
  status: "claimable" | "active" | "partially_exercised" | "fully_exercised";
};

export type DashboardSpots = Record<string, number>;

export type DashboardTotals = {
  ownedNotional: number;
  ownedPaid: number;
  ownedPnl: number;
  writtenNotional: number;
  premiumEarned: number;
  claimable: number;
  exercisable: number;
};

export type DashboardModal =
  | { kind: "exercise"; stage: ConfirmStage | "review"; position: OwnedPosition; qty: number }
  | { kind: "claim"; stage: ConfirmStage | "review"; position: WrittenPosition }
  | { kind: "close_early"; stage: ConfirmStage | "review"; position: WrittenPosition }
  | null;

// Activity

export type EventValue = { delta: number; unit: string };

export type EventStatus = "pending" | "confirmed" | "expired" | "reverted" | "info";

export type ActivityEvent = {
  id: string;
  ts: string;
  type: string;
  side: Side;
  status: EventStatus;
  title: string;
  body: string;
  value?: EventValue;
  txHash: string | null;
  bucket?: string;
};

export type ActivityTotals = {
  exercises: number;
  writes: number;
  buys: number;
  deposits: number;
  premiumIn: number;
  premiumOut: number;
};

export type GroupedEvent =
  | { kind: "day"; key: string; date: Date }
  | { kind: "event"; e: ActivityEvent };
