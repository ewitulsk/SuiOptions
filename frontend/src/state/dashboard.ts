// Real on-chain + indexer-backed dashboard state. Drop-in replacement for
// the seed data in `mocks/dashboard.ts`; returns the same `DashboardState`
// shape so `Dashboard.tsx`, `PositionCards.tsx`, and `ActionModal.tsx`
// stay unchanged.
//
// Wiring:
//   - written positions   ← api-service /positions (indexer-backed)
//   - owned call options  ← suiClient.getOwnedObjects (wallet)
//   - owned-call provenance ← api-service /call-token-lots (indexer)
//   - bucket cursors      ← api-service /buckets (5s refetch)
//   - spot                ← Pyth live feeds (BTC + SUI)
//
// `submit` builds + signs a PTB via dapp-kit, then transitions the modal
// through `signing → broadcast → confirmed`.

import { useEffect, useMemo, useState } from "react";
import {
  useCurrentAccount,
  useSignAndExecuteTransaction,
} from "@mysten/dapp-kit";

import { useBuckets } from "../api/useBuckets";
import { useCallTokenLots } from "../api/useCallTokenLots";
import { useOwnedCallOptions } from "../api/useOwnedCallOptions";
import { usePositions } from "../api/usePositions";
import { usePythPrice } from "../api/usePythPrice";
import type { CallTokenLot, Position, Series } from "../api/client";
import type { OwnedCallOption } from "../api/useOwnedCallOptions";
import { buildExerciseTx, buildRedeemTx } from "../tx/dashboard";
import type {
  DashboardModal,
  DashboardSpots,
  DashboardTotals,
  OwnedPosition,
  WrittenPosition,
} from "../types";

// ── helpers ───────────────────────────────────────────────────────────

const MS_PER_DAY = 1000 * 60 * 60 * 24;

function daysUntil(expiryMs: number, now: number): number {
  return Math.round((expiryMs - now) / MS_PER_DAY);
}

function isoDate(ms: number): string {
  return new Date(ms).toISOString().slice(0, 10);
}

/**
 * Strip a leading "T" from testnet test-token aliases so the existing
 * `PositionCards` styling (`asset === "BTC"`) lights up. Keep raw if it
 * doesn't match a known pair — better to show a slightly off label than
 * silently relabel something we don't recognise.
 */
function displayAsset(symbol: string): string {
  if (symbol === "TBTC") return "BTC";
  if (symbol === "TUSDC") return "USDC";
  return symbol;
}

export function shortAccount(id: string): string {
  const s = id.startsWith("0x") ? id.slice(2) : id;
  if (s.length <= 8) return `0x${s}`;
  return `0x${s.slice(0, 4)}…${s.slice(-4)}`;
}

function scaleU64(raw: string, decimals: number | null): number {
  if (decimals === null) return Number(raw);
  return Number(raw) / 10 ** decimals;
}

function scaleU128(raw: string, decimals: number | null): number {
  // BigInt → Number loses precision past 2^53, but every covered-call
  // amount we'll display fits in 2^53 even with 8 decimals. The raw
  // string remains available for any precision-sensitive call site.
  if (decimals === null) return Number(raw);
  const big = BigInt(raw);
  return Number(big) / 10 ** decimals;
}

/**
 * Find the bucket-level cursor + total_written via `/buckets`. The
 * `/positions` response carries a snapshot at fetch time; for the live
 * rangebar we prefer the `/buckets` 5s refetch which an Exercised event
 * elsewhere will keep current.
 */
function lookupBucketCursor(
  buckets: Series[] | undefined,
  bucketId: string,
): { totalWrittenRaw: string; cursorRaw: string } | null {
  if (!buckets) return null;
  for (const s of buckets) {
    for (const b of s.buckets) {
      if (b.bucket_id === bucketId)
        return {
          totalWrittenRaw: b.total_written_raw,
          cursorRaw: b.exercise_cursor_raw,
        };
    }
  }
  return null;
}

// ── owned-row construction ────────────────────────────────────────────

/**
 * Group call-token lots by bucket so a user's split CallOptions (which
 * don't have their own WriteExecuted) can still attribute provenance to
 * "your purchases in this bucket".
 */
function indexLotsByBucket(lots: CallTokenLot[]): Map<string, CallTokenLot[]> {
  const m = new Map<string, CallTokenLot[]>();
  for (const lot of lots) {
    const arr = m.get(lot.bucket_id);
    if (arr) arr.push(lot);
    else m.set(lot.bucket_id, [lot]);
  }
  return m;
}

function indexLotsByCallId(lots: CallTokenLot[]): Map<string, CallTokenLot> {
  const m = new Map<string, CallTokenLot>();
  for (const lot of lots) m.set(lot.call_option_id, lot);
  return m;
}

function indexBucketSeries(buckets: Series[] | undefined): Map<
  string,
  { series: Series; bucket: Series["buckets"][number] }
> {
  const m = new Map<string, { series: Series; bucket: Series["buckets"][number] }>();
  if (!buckets) return m;
  for (const s of buckets) {
    for (const b of s.buckets) m.set(b.bucket_id, { series: s, bucket: b });
  }
  return m;
}

function buildOwnedRow(
  obj: OwnedCallOption,
  lotByCallId: Map<string, CallTokenLot>,
  lotsByBucket: Map<string, CallTokenLot[]>,
  bucketIdx: Map<string, { series: Series; bucket: Series["buckets"][number] }>,
  spot: number,
  now: number,
): OwnedPosition | null {
  const bucketInfo = bucketIdx.get(obj.bucket_id);
  if (!bucketInfo) return null;
  const { series } = bucketInfo;

  // Provenance: prefer the exact lot when ids match. Fall back to a
  // bucket-aggregate (most recent lot) when the user split a call object
  // — the split child's id doesn't appear in any WriteExecuted.
  const directLot = lotByCallId.get(obj.object_id);
  const bucketLots = lotsByBucket.get(obj.bucket_id) ?? [];
  const aggregateLot = bucketLots.length > 0 ? bucketLots[0] : null;
  const provenance = directLot ?? aggregateLot;

  const amount = scaleU64(obj.amount_raw, series.asset_decimals);
  const strike = series.buckets.find((b) => b.bucket_id === obj.bucket_id)?.strike ?? 0;
  const dte = daysUntil(series.expiry_ms, now);
  const expired = dte < 0;
  const itm = spot > 0 && spot > strike;
  const moneyness = strike > 0 ? ((spot - strike) / strike) * 100 : 0;
  const intrinsicNow = Math.max(0, (spot - strike) * amount);
  // Premium attribution: if the lot's `amount` doesn't match the
  // current object's amount (split), pro-rate. Best-effort; not exact.
  const lotAmount = provenance
    ? scaleU64(provenance.amount_raw, series.asset_decimals)
    : amount;
  const lotPremium = provenance
    ? scaleU64(provenance.premium_paid_raw, series.settlement_decimals)
    : 0;
  const premiumPaid = lotAmount > 0 ? (lotPremium * amount) / lotAmount : 0;
  const pnl = intrinsicNow - premiumPaid;
  const status: OwnedPosition["status"] = expired
    ? itm
      ? "expired_itm"
      : "expired_otm"
    : itm
      ? "exercisable"
      : "active_otm";

  return {
    id: obj.object_id,
    side: "owned",
    asset: displayAsset(series.asset_symbol),
    strike,
    expiry: isoDate(series.expiry_ms),
    amount,
    premiumPaid,
    boughtFrom: provenance ? shortAccount(provenance.seller_account_id) : "—",
    boughtAt: provenance ? isoDate(provenance.timestamp_ms) : "",
    rangeId: shortAccount(obj.object_id),
    spot,
    dte,
    itm,
    moneyness,
    intrinsicNow,
    pnl,
    status,
  };
}

// ── written-row construction ──────────────────────────────────────────

function buildWrittenRow(
  p: Position,
  liveCursor: { totalWrittenRaw: string; cursorRaw: string } | null,
  spot: number,
  now: number,
): WrittenPosition {
  const amount = scaleU128(
    (BigInt(p.range_end_raw) - BigInt(p.range_start_raw)).toString(),
    p.asset_decimals,
  );
  const rangeStart = scaleU128(p.range_start_raw, p.asset_decimals);
  const rangeEnd = scaleU128(p.range_end_raw, p.asset_decimals);
  const cursorRaw = liveCursor?.cursorRaw ?? p.exercise_cursor_raw;
  const cursor = scaleU128(cursorRaw, p.asset_decimals);

  const totalQty = rangeEnd - rangeStart;
  const exercisedQty = Math.max(
    0,
    Math.min(cursor, rangeEnd) - rangeStart,
  );
  const exercisedPct = totalQty > 0 ? (exercisedQty / totalQty) * 100 : 0;

  const strike = p.strike ?? 0;
  const dte = daysUntil(p.expiry_ms, now);
  const expired = dte < 0;
  const status: WrittenPosition["status"] = expired
    ? "claimable"
    : exercisedQty >= totalQty && totalQty > 0
      ? "fully_exercised"
      : exercisedQty > 0
        ? "partially_exercised"
        : "active";

  const premiumReceived = scaleU64(
    p.premium_received_raw,
    p.settlement_decimals,
  );

  return {
    id: p.position_object_id,
    side: "written",
    asset: displayAsset(p.asset_symbol),
    strike,
    expiry: isoDate(p.expiry_ms),
    amount,
    premiumReceived,
    soldTo: shortAccount(p.mm_account_id),
    soldAt: isoDate(p.minted_at_ms),
    rangeStart,
    rangeEnd,
    spot,
    dte,
    exercisedQty,
    totalQty,
    exercisedPct,
    cursor,
    status,
  };
}

// ── the hook ──────────────────────────────────────────────────────────

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
  submit: () => Promise<void>;
  closeModal: () => void;
  toast: string | null;
  connected: boolean;
  /** Connected wallet address, or null when disconnected. */
  address: string | null;
};

export function useDashboardState(): DashboardState {
  const account = useCurrentAccount();
  const wallet = account?.address ?? null;
  const connected = wallet !== null;

  const positions = usePositions(wallet);
  const lots = useCallTokenLots(wallet);
  const owned = useOwnedCallOptions(wallet);
  const buckets = useBuckets();

  const btcLive = usePythPrice("BTC");
  const suiLive = usePythPrice("SUI");
  const spots = useMemo<DashboardSpots>(
    () => ({
      BTC: btcLive?.price ?? null,
      SUI: suiLive?.price ?? null,
    }),
    [btcLive?.price, suiLive?.price],
  );

  const [tab, setTab] = useState<"owned" | "written">("owned");
  const [modal, setModal] = useState<DashboardModal>(null);
  const [toast, setToast] = useState<string | null>(null);

  // Re-tick once a minute so `dte` rolls over without a manual refresh.
  const [now, setNow] = useState<number>(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 60_000);
    return () => clearInterval(id);
  }, []);

  // ── rows ──────────────────────────────────────────────────────────

  const ownedRows = useMemo<OwnedPosition[]>(() => {
    const ownedObjs = owned.data ?? [];
    const lotsArr = lots.data ?? [];
    const lotByCallId = indexLotsByCallId(lotsArr);
    const lotsByBucket = indexLotsByBucket(lotsArr);
    const bucketIdx = indexBucketSeries(buckets.data);

    return ownedObjs
      .map((o) => {
        const series = bucketIdx.get(o.bucket_id)?.series;
        const symbol = displayAsset(series?.asset_symbol ?? "");
        const spot = spots[symbol] ?? 0;
        return buildOwnedRow(o, lotByCallId, lotsByBucket, bucketIdx, spot, now);
      })
      .filter((r): r is OwnedPosition => r !== null);
  }, [owned.data, lots.data, buckets.data, spots, now]);

  const writtenRows = useMemo<WrittenPosition[]>(() => {
    const ps = positions.data ?? [];
    return ps.map((p) => {
      const liveCursor = lookupBucketCursor(buckets.data, p.bucket_id);
      const symbol = displayAsset(p.asset_symbol);
      const spot = spots[symbol] ?? 0;
      return buildWrittenRow(p, liveCursor, spot, now);
    });
  }, [positions.data, buckets.data, spots, now]);

  // ── totals ────────────────────────────────────────────────────────

  const totals = useMemo<DashboardTotals>(() => {
    const ownedNotional = ownedRows.reduce(
      (sum, p) => sum + p.spot * p.amount,
      0,
    );
    const ownedPaid = ownedRows.reduce((sum, p) => sum + p.premiumPaid, 0);
    const ownedPnl = ownedRows.reduce((sum, p) => sum + p.pnl, 0);
    const writtenNotional = writtenRows.reduce(
      (sum, p) => sum + p.spot * (p.totalQty || 0),
      0,
    );
    const premiumEarned = writtenRows.reduce(
      (sum, p) => sum + p.premiumReceived,
      0,
    );
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

  // ── action modal ──────────────────────────────────────────────────

  const openExercise = (p: OwnedPosition, qty?: number) => {
    setModal({
      kind: "exercise",
      stage: "review",
      position: p,
      qty: qty ?? p.amount,
    });
  };
  const openClaim = (p: WrittenPosition) => {
    setModal({ kind: "claim", stage: "review", position: p });
  };

  const { mutateAsync: signAndExecute } = useSignAndExecuteTransaction();

  const submit = async () => {
    if (!modal || !wallet) return;
    const captured = modal;
    setModal({ ...captured, stage: "signing" } as DashboardModal);

    try {
      if (captured.kind === "exercise") {
        const { position: p, qty } = captured;
        const ownedObj = (owned.data ?? []).find((o) => o.object_id === p.id);
        const bucketInfo = (buckets.data ?? [])
          .flatMap((s) => s.buckets.map((b) => ({ series: s, b })))
          .find((x) => x.b.bucket_id === ownedObj?.bucket_id);
        if (!ownedObj || !bucketInfo) {
          throw new Error("missing on-chain reference for exercise");
        }
        const { series, b: bucket } = bucketInfo;
        const assetDec = series.asset_decimals ?? 0;
        const fullAmountRaw = BigInt(ownedObj.amount_raw);
        const exerciseAmountRaw = BigInt(
          Math.round(qty * 10 ** assetDec).toString(),
        );
        // settlement_raw = exerciseAmount_raw * strike_raw / 10^strike_scale
        // (strike_raw / 10^strike_scale is settlement-smallest per
        // underlying-smallest; multiplying by the raw amount gives raw
        // settlement.)
        const strikeRaw = BigInt(bucket.strike_raw);
        const scale = BigInt(10) ** BigInt(bucket.strike_scale);
        const settlementAmountRaw = (exerciseAmountRaw * strikeRaw) / scale;

        const tx = buildExerciseTx({
          bucketId: ownedObj.bucket_id,
          callOptionId: ownedObj.object_id,
          fullAmountRaw,
          exerciseAmountRaw,
          settlementAmountRaw,
          underlyingCoinType: series.asset_coin_type,
          settlementCoinType: series.settlement_coin_type,
          recipient: wallet,
        });
        setModal({ ...captured, stage: "broadcast" } as DashboardModal);
        await signAndExecute({ transaction: tx });
        setModal({ ...captured, stage: "confirmed" } as DashboardModal);
      } else if (captured.kind === "claim") {
        const { position: p } = captured;
        const matchPos = (positions.data ?? []).find(
          (pp) => pp.position_object_id === p.id,
        );
        if (!matchPos) throw new Error("position not found in /positions");
        const tx = buildRedeemTx({
          bucketId: matchPos.bucket_id,
          positionObjectId: matchPos.position_object_id,
          underlyingCoinType: matchPos.asset_coin_type,
          settlementCoinType: matchPos.settlement_coin_type,
          recipient: wallet,
        });
        setModal({ ...captured, stage: "broadcast" } as DashboardModal);
        await signAndExecute({ transaction: tx });
        setModal({ ...captured, stage: "confirmed" } as DashboardModal);
      }
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setToast(`failed · ${message}`);
      setTimeout(() => setToast(null), 6000);
      setModal(null);
    }
  };

  const closeModal = () => {
    if (modal && (modal as { stage?: string }).stage === "confirmed") {
      const k = modal.kind;
      if (k === "exercise") {
        setToast(
          `exercised · received ${modal.qty.toFixed(modal.position.asset === "BTC" ? 4 : 0)} ${modal.position.asset}`,
        );
      } else if (k === "claim") {
        setToast("claimed · position closed");
      }
      setTimeout(() => setToast(null), 4500);
    }
    setModal(null);
    // Trigger refetches so the rows reflect the new on-chain state.
    positions.refetch();
    lots.refetch();
    owned.refetch();
  };

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
    submit,
    closeModal,
    toast,
    connected,
    address: wallet,
  };
}
