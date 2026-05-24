// Mock dashboard state — drop-in seam for on-chain positions, bucket cursors,
// and exercise/claim flows. Real implementation should fetch from Sui RPC +
// indexer and submit transactions via wallet.
import { useMemo, useState } from "react";
import type {
  DashboardModal,
  DashboardSpots,
  DashboardTotals,
  OwnedPosition,
  WrittenPosition,
} from "../types";
import { usePythPrice } from "../api/usePythPrice";

const TODAY = new Date("2026-05-16");

function daysUntil(dateStr: string): number {
  const d = new Date(dateStr);
  return Math.round((d.getTime() - TODAY.getTime()) / (1000 * 60 * 60 * 24));
}

type SeedOwned = Omit<
  OwnedPosition,
  "spot" | "dte" | "itm" | "moneyness" | "intrinsicNow" | "pnl" | "status"
>;
type SeedWritten = Omit<
  WrittenPosition,
  "spot" | "dte" | "exercisedQty" | "totalQty" | "exercisedPct" | "cursor" | "status"
>;

const SEED_OWNED: SeedOwned[] = [
  { id: "pos_t_01", side: "owned", asset: "BTC", strike: 85000, expiry: "2026-06-26", amount: 0.05,  premiumPaid: 91.38, boughtFrom: "argonaut_mm_01", boughtAt: "2026-05-08", rangeId: "0x9a3f…0042" },
  { id: "pos_t_02", side: "owned", asset: "BTC", strike: 94000, expiry: "2026-06-26", amount: 0.02,  premiumPaid: 26.10, boughtFrom: "tidal_quants",   boughtAt: "2026-05-11", rangeId: "0xb1c2…0103" },
  { id: "pos_t_03", side: "owned", asset: "SUI", strike: 4.50,  expiry: "2026-06-26", amount: 400,   premiumPaid: 28.40, boughtFrom: "phocas_capital", boughtAt: "2026-05-09", rangeId: "0xc8e0…02ab" },
  { id: "pos_t_04", side: "owned", asset: "BTC", strike: 90000, expiry: "2026-05-29", amount: 0.01,  premiumPaid:  4.21, boughtFrom: "argonaut_mm_01", boughtAt: "2026-04-30", rangeId: "0xa14b…0058" },
];

const SEED_WRITTEN: SeedWritten[] = [
  { id: "pos_w_01", side: "written", asset: "BTC", strike: 85000, expiry: "2026-06-26", amount: 0.05,   premiumReceived: 86.72,  soldTo: "argonaut_mm_01", soldAt: "2026-05-04", rangeStart: 0.84,  rangeEnd: 0.89,  cursorAtSale: 0.42 },
  { id: "pos_w_02", side: "written", asset: "SUI", strike: 4.20,  expiry: "2026-06-26", amount: 200,    premiumReceived: 18.60,  soldTo: "phocas_capital", soldAt: "2026-05-09", rangeStart: 1820,  rangeEnd: 2020,  cursorAtSale: 1400 },
  { id: "pos_w_03", side: "written", asset: "BTC", strike: 80000, expiry: "2026-04-24", amount: 0.1,    premiumReceived: 142.40, soldTo: "tidal_quants",   soldAt: "2026-03-18", rangeStart: 0.2,   rangeEnd: 0.3,   cursorAtExpiry: 0.235 },
];

const BUCKET_KEY = (p: { asset: string; strike: number; expiry: string }) =>
  `${p.asset}-${p.strike}-${p.expiry}`;

const SEED_CURSORS: Record<string, number> = {
  "BTC-85000-2026-06-26": 0.86,
  "BTC-94000-2026-06-26": 0.0,
  "SUI-4.5-2026-06-26": 0,
  "SUI-4.2-2026-06-26": 1880,
  "BTC-90000-2026-05-29": 0.0,
  "BTC-80000-2026-04-24": 0.235,
};

export type DashboardState = {
  tab: "owned" | "written";
  setTab: (t: "owned" | "written") => void;
  ownedRows: OwnedPosition[];
  writtenRows: WrittenPosition[];
  spots: DashboardSpots;
  totals: DashboardTotals;
  modal: DashboardModal;
  openExercise: (p: OwnedPosition, qty?: number) => void;
  openClaim: (p: WrittenPosition) => void;
  openCloseEarly: (p: WrittenPosition) => void;
  submit: () => void;
  closeModal: () => void;
  toast: string | null;
};

export function useDashboardState(): DashboardState {
  const [ownedSeed, setOwnedSeed] = useState<SeedOwned[]>(SEED_OWNED);
  const [writtenSeed, setWrittenSeed] = useState<SeedWritten[]>(SEED_WRITTEN);
  const [cursors, setCursors] = useState<Record<string, number>>(SEED_CURSORS);
  const [tab, setTab] = useState<"owned" | "written">("owned");
  const [modal, setModal] = useState<DashboardModal>(null);
  const [toast, setToast] = useState<string | null>(null);

  // Two hooks → two subscriptions on the shared Pyth singleton. The client
  // coalesces them into a single Hermes stream containing both feed ids; if
  // /buy or /earn is also mounted with `TBTC`, that screen shares the BTC
  // feed via the same connection (TBTC and BTC alias to the same feed id).
  const btcLive = usePythPrice("BTC");
  const suiLive = usePythPrice("SUI");
  const spots = useMemo<DashboardSpots>(
    () => ({
      BTC: btcLive?.price ?? null,
      SUI: suiLive?.price ?? null,
    }),
    [btcLive?.price, suiLive?.price],
  );

  const ownedRows = useMemo<OwnedPosition[]>(
    () =>
      ownedSeed.map((p) => {
        const spot = spots[p.asset] ?? 0;
        const dte = daysUntil(p.expiry);
        const expired = dte < 0;
        const itm = spot > p.strike;
        const moneyness = ((spot - p.strike) / p.strike) * 100;
        const intrinsicNow = Math.max(0, (spot - p.strike) * p.amount);
        const pnl = intrinsicNow - p.premiumPaid;
        const status: OwnedPosition["status"] = expired
          ? itm
            ? "expired_itm"
            : "expired_otm"
          : itm
            ? "exercisable"
            : "active_otm";
        return { ...p, spot, dte, itm, moneyness, intrinsicNow, pnl, status };
      }),
    [ownedSeed, spots],
  );

  const writtenRows = useMemo<WrittenPosition[]>(
    () =>
      writtenSeed.map((p) => {
        const spot = spots[p.asset] ?? 0;
        const dte = daysUntil(p.expiry);
        const expired = dte < 0;
        const cursor = cursors[BUCKET_KEY(p)] ?? 0;
        const exercisedQty = Math.max(0, Math.min(cursor, p.rangeEnd) - p.rangeStart);
        const totalQty = p.rangeEnd - p.rangeStart;
        const exercisedPct = (exercisedQty / totalQty) * 100;
        const status: WrittenPosition["status"] = expired
          ? "claimable"
          : exercisedQty >= totalQty
            ? "fully_exercised"
            : exercisedQty > 0
              ? "partially_exercised"
              : "active";
        return {
          ...p,
          spot,
          dte,
          exercisedQty,
          totalQty,
          exercisedPct,
          cursor,
          status,
        };
      }),
    [writtenSeed, cursors, spots],
  );

  const openExercise = (p: OwnedPosition, qty?: number) => {
    setModal({ kind: "exercise", stage: "review", position: p, qty: qty ?? p.amount });
  };
  const openClaim = (p: WrittenPosition) => {
    setModal({ kind: "claim", stage: "review", position: p });
  };
  const openCloseEarly = (p: WrittenPosition) => {
    setModal({ kind: "close_early", stage: "review", position: p });
  };

  const submit = () => {
    if (!modal) return;
    const captured = modal;
    setModal({ ...captured, stage: "signing" } as DashboardModal);
    setTimeout(() => {
      setModal((m) => (m ? ({ ...m, stage: "broadcast" } as DashboardModal) : null));
    }, 900);
    setTimeout(() => {
      setModal((m) => (m ? ({ ...m, stage: "confirmed" } as DashboardModal) : null));
      if (captured.kind === "exercise") {
        const { position: p, qty } = captured;
        const k = BUCKET_KEY(p);
        setCursors((c) => ({ ...c, [k]: (c[k] || 0) + qty }));
        setOwnedSeed((ps) =>
          ps.flatMap((x) => {
            if (x.id !== p.id) return [x];
            const rem = x.amount - qty;
            return rem <= 1e-9 ? [] : [{ ...x, amount: rem }];
          }),
        );
      } else if (captured.kind === "claim") {
        const id = captured.position.id;
        setWrittenSeed((ps) => ps.filter((x) => x.id !== id));
      } else if (captured.kind === "close_early") {
        const id = captured.position.id;
        setWrittenSeed((ps) => ps.filter((x) => x.id !== id));
      }
    }, 2200);
  };

  const closeModal = () => {
    if (modal && (modal as { stage?: string }).stage === "confirmed") {
      const k = modal.kind;
      if (k === "exercise") {
        setToast(
          `exercised · received ${modal.qty.toFixed(4)} ${modal.position.asset}`,
        );
      } else if (k === "claim") {
        setToast(`claimed · position closed`);
      } else if (k === "close_early") {
        setToast(`closed early · proceeds received`);
      }
      setTimeout(() => setToast(null), 4500);
    }
    setModal(null);
  };

  const totals = useMemo<DashboardTotals>(() => {
    const ownedNotional = ownedRows.reduce((sum, p) => sum + p.spot * p.amount, 0);
    const ownedPaid = ownedRows.reduce((sum, p) => sum + p.premiumPaid, 0);
    const ownedPnl = ownedRows.reduce((sum, p) => sum + p.pnl, 0);
    const writtenNotional = writtenRows.reduce(
      (sum, p) => sum + p.spot * (p.totalQty || 0),
      0,
    );
    const premiumEarned = writtenRows.reduce((sum, p) => sum + p.premiumReceived, 0);
    const claimable = writtenRows.filter((p) => p.status === "claimable").length;
    const exercisable = ownedRows.filter((p) => p.status === "exercisable").length;
    return {
      ownedNotional,
      ownedPaid,
      ownedPnl,
      writtenNotional,
      premiumEarned,
      claimable,
      exercisable,
    };
  }, [ownedRows, writtenRows]);

  return {
    tab,
    setTab,
    ownedRows,
    writtenRows,
    spots,
    totals,
    modal,
    openExercise,
    openClaim,
    openCloseEarly,
    submit,
    closeModal,
    toast,
  };
}
