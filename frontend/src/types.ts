// Shared domain types. UI components consume these — mocks in `src/mocks/`
// produce them, but a real Sui SDK / indexer layer can produce the same shapes.

export type Side = "writer" | "trader" | "account";
export type View = "writer" | "trader";
/** Cash-secured PUT vs covered CALL. Orthogonal to writer/trader `View`. */
export type OptionType = "call" | "put";

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
  optionType: OptionType;
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

/** One FIFO cost lot of a held option (SO-209). A `transfer` lot is a token
 *  the user acquired off-venue (transfer/gift) at $0 cost — its own row. */
export type OwnedLot = {
  amount: number;
  cost: number;
  source: "rfq" | "deepbook" | "transfer";
  acquiredAtMs: number;
};

export type OwnedPosition = {
  id: string;
  side: "owned";
  /** "call" (covered call) or "put" (cash-secured put). Defaults to "call". */
  optionType: OptionType;
  asset: "BTC" | "SUI" | string;
  strike: number;
  expiry: string;
  amount: number;
  /** Cost basis of the currently-held lots (settlement units). Premium paid
   *  for RFQ lots + amount paid for DeepBook buys; $0 for transferred-in. */
  premiumPaid: number;
  boughtFrom: string;
  boughtAt: string;
  rangeId: string;
  /** FIFO cost lots making up the current holding (incl. $0 transfer lots). */
  lots: OwnedLot[];
  // decorated
  spot: number;
  dte: number;
  itm: boolean;
  moneyness: number;
  intrinsicNow: number;
  /** Unrealized PnL if exercised now: `intrinsicNow − costBasis`. */
  pnl: number;
  /** Realized PnL from past DeepBook sells / exercises / expiries in this bucket. */
  realizedPnl: number;
  /** `realizedPnl + pnl` — true options PnL to date. */
  totalPnl: number;
  /** Exercised tokens that couldn't be priced (excluded from realizedPnl). */
  unpricedExerciseAmount: number;
  status:
    | "exercisable"
    | "active_otm"
    | "expired_itm"
    | "expired_otm";
};

export type WrittenPosition = {
  id: string;
  side: "written";
  /** "call" (covered call) or "put" (cash-secured put). Defaults to "call". */
  optionType: OptionType;
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

/** `null` means the live feed for that asset hasn't delivered (or has dropped). */
export type DashboardSpots = Record<string, number | null>;

export type DashboardTotals = {
  ownedNotional: number;
  ownedPaid: number;
  ownedPnl: number;
  /** Realized PnL across owned buckets (closed sells/exercises/expiries). */
  ownedRealized: number;
  /** ownedRealized + ownedPnl — total options PnL to date. */
  ownedTotalPnl: number;
  writtenNotional: number;
  premiumEarned: number;
  claimable: number;
  exercisable: number;
};

export type DashboardModal =
  | { kind: "exercise"; stage: ConfirmStage | "review"; position: OwnedPosition; qty: number }
  | { kind: "claim"; stage: ConfirmStage | "review"; position: WrittenPosition }
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
