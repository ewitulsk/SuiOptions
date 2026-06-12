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
import { useQuery } from "@tanstack/react-query";
import { addSessionExercise, addSessionRedeem } from "../tx/session";
import { useUserIdentity } from "../session/identity";
import { executeWithSession } from "../session/store";
import { useSubmitTransaction } from "../tx/submit";
import { posthog } from "../lib/posthog";

import type { ToastState } from "../components/Toast";
import { useBuckets } from "../api/useBuckets";
import { useOwnedCallOptions } from "../api/useOwnedCallOptions";
import { useCallTokenLots } from "../api/useCallTokenLots";
import { useOwnedPositions, type OwnedPositionObj } from "../api/useOwnedPositions";
import { usePythPrices } from "../api/usePythPrice";
import { fetchEnrichedPositions } from "../api/client";
import type { CallTokenLot, EnrichedPosition, Position, Series } from "../api/client";
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

/**
 * SO-97: synthesize a `Position` for a wallet-held object the indexer hasn't
 * enriched yet (lag, or its bucket aged out of the read model). Structural
 * fields come from the wallet object; bucket fields fall back to `/buckets`
 * when available. Provenance (premium / MM / minted-at) is unknown — left
 * blank so the row renders degraded rather than disappearing.
 */
function degradedPosition(
  o: OwnedPositionObj,
  info: { series: Series; bucket: Series["buckets"][number] } | undefined,
): Position {
  const s = info?.series;
  const b = info?.bucket;
  return {
    position_object_id: o.object_id,
    bucket_id: o.bucket_id,
    asset_symbol: s?.asset_symbol ?? "",
    asset_decimals: s?.asset_decimals ?? null,
    asset_coin_type: s?.asset_coin_type ?? "",
    settlement_symbol: s?.settlement_symbol ?? "USDC",
    settlement_decimals: s?.settlement_decimals ?? null,
    settlement_coin_type: s?.settlement_coin_type ?? "",
    strike: b?.strike ?? 0,
    strike_raw: b?.strike_raw ?? "0",
    strike_scale: b?.strike_scale ?? 0,
    expiry_ms: s?.expiry_ms ?? Date.now(),
    range_start_raw: o.range_start_raw,
    range_end_raw: o.range_end_raw,
    total_written_raw: b?.total_written_raw ?? o.range_end_raw,
    exercise_cursor_raw: b?.exercise_cursor_raw ?? "0",
    premium_received_raw: "0",
    mm_account_id: "",
    minted_at_ms: 0,
  };
}

function buildOwnedRow(
  obj: OwnedCallOption,
  lotsByBucket: Map<string, CallTokenLot[]>,
  bucketIdx: Map<string, { series: Series; bucket: Series["buckets"][number] }>,
  spot: number,
  now: number,
): OwnedPosition | null {
  const bucketInfo = bucketIdx.get(obj.bucket_id);
  if (!bucketInfo) return null;
  const { series } = bucketInfo;

  // Provenance: option coins are fungible (no per-object id), so we attribute
  // at the bucket level — all WriteExecuted lots for this bucket. `bucketLots`
  // is newest-first; `provenance` (the most recent) drives the single-value
  // `boughtFrom`/`boughtAt` display, while `premiumPaid` aggregates every lot.
  const bucketLots = lotsByBucket.get(obj.bucket_id) ?? [];
  const provenance = bucketLots.length > 0 ? bucketLots[0] : null;

  const amount = scaleU64(obj.amount_raw, series.asset_decimals);
  const strike = series.buckets.find((b) => b.bucket_id === obj.bucket_id)?.strike ?? 0;
  const dte = daysUntil(series.expiry_ms, now);
  const expired = dte < 0;
  const itm = spot > 0 && spot > strike;
  const moneyness = strike > 0 ? ((spot - strike) / strike) * 100 : 0;
  const intrinsicNow = Math.max(0, (spot - strike) * amount);
  // Premium attribution (true cost basis): sum premium + amount across all
  // lots for this bucket, then pro-rate to the currently-held `amount`. The
  // pro-rate covers the case where holdings don't match lot totals (splits,
  // partial transfers). Best-effort; not exact for split children.
  const totalLotAmount = bucketLots.reduce(
    (sum, lot) => sum + scaleU64(lot.amount_raw, series.asset_decimals),
    0,
  );
  const totalLotPremium = bucketLots.reduce(
    (sum, lot) => sum + scaleU64(lot.premium_paid_raw, series.settlement_decimals),
    0,
  );
  const premiumPaid =
    totalLotAmount > 0 ? (totalLotPremium * amount) / totalLotAmount : 0;
  const pnl = intrinsicNow - premiumPaid;
  const status: OwnedPosition["status"] = expired
    ? itm
      ? "expired_itm"
      : "expired_otm"
    : itm
      ? "exercisable"
      : "active_otm";

  return {
    id: obj.coin_type,
    side: "owned",
    asset: displayAsset(series.asset_symbol),
    strike,
    expiry: isoDate(series.expiry_ms),
    amount,
    premiumPaid,
    boughtFrom: provenance ? shortAccount(provenance.seller_account_id) : "—",
    boughtAt: provenance ? isoDate(provenance.timestamp_ms) : "",
    rangeId: shortAccount(obj.bucket_id),
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
    soldTo: p.mm_account_id ? shortAccount(p.mm_account_id) : "—",
    soldAt: p.minted_at_ms ? isoDate(p.minted_at_ms) : "",
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
  toast: ToastState | null;
  connected: boolean;
  /** Connected wallet address, or null when disconnected. */
  address: string | null;
};

export function useDashboardState(): DashboardState {
  const identity = useUserIdentity();
  const sessionState = identity?.kind === "session" ? identity.session : null;
  const wallet = identity?.address ?? null;
  const connected = wallet !== null;

  // Written positions: the wallet is the authoritative list (transfer-correct,
  // no projection snapshot bound), enriched by object id via api-service.
  const ownedPositions = useOwnedPositions(
    wallet,
    sessionState ? sessionState.positionIds : null,
  );
  const buckets = useBuckets();
  // Option coins live in per-roll packages; the bucket list supplies the
  // coin-type→bucket mapping the owned-call query needs.
  const owned = useOwnedCallOptions(
    wallet,
    buckets.data,
    sessionState ? sessionState.balances : null,
  );
  // Per-purchase provenance for owned calls (boughtFrom / premiumPaid / boughtAt).
  const lots = useCallTokenLots(wallet);

  const positionIds = useMemo(
    () => (ownedPositions.data ?? []).map((p) => p.object_id).sort(),
    [ownedPositions.data],
  );
  const enriched = useQuery<EnrichedPosition[], Error>({
    queryKey: ["enriched-positions", positionIds],
    enabled: positionIds.length > 0,
    refetchInterval: 5_000,
    queryFn: () => fetchEnrichedPositions(positionIds),
  });

  // Spot feeds follow the assets the protocol actually offers — one Pyth
  // subscription per distinct display-asset across all series, keyed by the
  // same display symbol the rows resolve against (`spots[p.asset]`).
  const assetSymbols = useMemo<string[]>(() => {
    const set = new Set<string>();
    for (const s of buckets.data ?? []) set.add(displayAsset(s.asset_symbol));
    return Array.from(set).sort();
  }, [buckets.data]);
  const livePrices = usePythPrices(assetSymbols);
  const spots = useMemo<DashboardSpots>(() => {
    const out: DashboardSpots = {};
    for (const sym of assetSymbols) out[sym] = livePrices[sym]?.price ?? null;
    return out;
  }, [assetSymbols, livePrices]);

  const [tab, setTab] = useState<"owned" | "written">("owned");
  const [modal, setModal] = useState<DashboardModal>(null);
  const [toast, setToast] = useState<ToastState | null>(null);

  // Re-tick once a minute so `dte` rolls over without a manual refresh.
  const [now, setNow] = useState<number>(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 60_000);
    return () => clearInterval(id);
  }, []);

  // ── rows ──────────────────────────────────────────────────────────

  const ownedRows = useMemo<OwnedPosition[]>(() => {
    const ownedObjs = owned.data ?? [];
    const bucketIdx = indexBucketSeries(buckets.data);
    // Provenance (boughtFrom / premiumPaid) is bucket-level: option coins are
    // fungible, so we group every WriteExecuted lot by its bucket. The backend
    // returns lots newest-first, preserved on insert.
    const lotsByBucket = new Map<string, CallTokenLot[]>();
    for (const lot of lots.data ?? []) {
      const list = lotsByBucket.get(lot.bucket_id);
      if (list) list.push(lot);
      else lotsByBucket.set(lot.bucket_id, [lot]);
    }

    return ownedObjs
      .map((o) => {
        const series = bucketIdx.get(o.bucket_id)?.series;
        const symbol = displayAsset(series?.asset_symbol ?? "");
        const spot = spots[symbol] ?? 0;
        return buildOwnedRow(o, lotsByBucket, bucketIdx, spot, now);
      })
      .filter((r): r is OwnedPosition => r !== null);
  }, [owned.data, buckets.data, lots.data, spots, now]);

  const writtenRows = useMemo<WrittenPosition[]>(() => {
    const objs = ownedPositions.data ?? [];
    const enrichedById = new Map(
      (enriched.data ?? []).map((e) => [e.position_object_id, e]),
    );
    const bucketIdx = indexBucketSeries(buckets.data);
    return objs.map((o) => {
      // Enriched rows are `Position`-shaped (a superset); degraded rows are
      // synthesized from the wallet object + /buckets so a held position
      // never silently disappears while the indexer catches up.
      const pos: Position =
        enrichedById.get(o.object_id) ?? degradedPosition(o, bucketIdx.get(o.bucket_id));
      const liveCursor = lookupBucketCursor(buckets.data, pos.bucket_id);
      const symbol = displayAsset(pos.asset_symbol);
      const spot = spots[symbol] ?? 0;
      return buildWrittenRow(pos, liveCursor, spot, now);
    });
  }, [ownedPositions.data, enriched.data, buckets.data, spots, now]);

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

  const submitTx = useSubmitTransaction();

  const submit = async () => {
    if (!modal || !wallet) return;
    const captured = modal;
    setModal({ ...captured, stage: "signing" } as DashboardModal);

    try {
      if (captured.kind === "exercise") {
        const { position: p, qty } = captured;
        const ownedObj = (owned.data ?? []).find((o) => o.coin_type === p.id);
        const bucketInfo = (buckets.data ?? [])
          .flatMap((s) => s.buckets.map((b) => ({ series: s, b })))
          .find((x) => x.b.bucket_id === ownedObj?.bucket_id);
        if (!ownedObj || !bucketInfo) {
          throw new Error("missing on-chain reference for exercise");
        }
        const { series, b: bucket } = bucketInfo;
        const assetDec = series.asset_decimals ?? 0;
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

        setModal({ ...captured, stage: "broadcast" } as DashboardModal);
        if (sessionState) {
          await executeWithSession("exercising", (tx, ctx) =>
            addSessionExercise(tx, ctx, {
              bucketId: ownedObj.bucket_id,
              callCoinType: ownedObj.coin_type,
              underlyingCoinType: series.asset_coin_type,
              settlementCoinType: series.settlement_coin_type,
              exerciseAmountRaw,
            }),
          );
        } else {
          const tx = buildExerciseTx({
            bucketId: ownedObj.bucket_id,
            callCoinType: ownedObj.coin_type,
            exerciseAmountRaw,
            settlementAmountRaw,
            underlyingCoinType: series.asset_coin_type,
            settlementCoinType: series.settlement_coin_type,
            recipient: wallet,
          });
          await submitTx(tx);
        }
        setModal({ ...captured, stage: "confirmed" } as DashboardModal);
        posthog.capture("option_exercised", {
          asset: captured.position.asset,
          strike: captured.position.strike,
          expiry: captured.position.expiry,
          qty,
          pnl: captured.position.pnl,
          wallet_address: wallet,
          auth: sessionState ? "session" : "wallet",
        });
      } else if (captured.kind === "claim") {
        const { position: p } = captured;
        // Resolve the on-chain object from the wallet (authoritative) and the
        // coin types from /buckets — no dependency on the /positions endpoint.
        const matchPos = (ownedPositions.data ?? []).find((pp) => pp.object_id === p.id);
        const bucketInfo = (buckets.data ?? [])
          .flatMap((s) => s.buckets.map((b) => ({ series: s, b })))
          .find((x) => x.b.bucket_id === matchPos?.bucket_id);
        if (!matchPos || !bucketInfo) {
          throw new Error("missing on-chain reference for claim");
        }
        setModal({ ...captured, stage: "broadcast" } as DashboardModal);
        if (sessionState) {
          await executeWithSession("redeeming", (tx, ctx) =>
            addSessionRedeem(tx, ctx, {
              bucketId: matchPos.bucket_id,
              positionObjectId: matchPos.object_id,
              callCoinType: bucketInfo.b.call_coin_type,
              underlyingCoinType: bucketInfo.series.asset_coin_type,
              settlementCoinType: bucketInfo.series.settlement_coin_type,
            }),
          );
        } else {
          const tx = buildRedeemTx({
            bucketId: matchPos.bucket_id,
            positionObjectId: matchPos.object_id,
            callCoinType: bucketInfo.b.call_coin_type,
            underlyingCoinType: bucketInfo.series.asset_coin_type,
            settlementCoinType: bucketInfo.series.settlement_coin_type,
            recipient: wallet,
          });
          await submitTx(tx);
        }
        setModal({ ...captured, stage: "confirmed" } as DashboardModal);
        posthog.capture("position_claimed", {
          asset: captured.position.asset,
          strike: captured.position.strike,
          expiry: captured.position.expiry,
          premium_received: captured.position.premiumReceived,
          exercised_pct: captured.position.exercisedPct,
          wallet_address: wallet,
          auth: sessionState ? "session" : "wallet",
        });
      }
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      posthog.captureException(err, {
        action: captured?.kind,
        wallet_address: wallet,
        auth: sessionState ? "session" : "wallet",
      });
      setToast({ message: `failed · ${message}`, variant: "error" });
      setTimeout(() => setToast(null), 6000);
      setModal(null);
    }
  };

  const closeModal = () => {
    if (modal && (modal as { stage?: string }).stage === "confirmed") {
      const k = modal.kind;
      if (k === "exercise") {
        setToast({
          message: `exercised · received ${modal.qty.toFixed(modal.position.asset === "BTC" ? 4 : 0)} ${modal.position.asset}`,
          variant: "success",
        });
      } else if (k === "claim") {
        setToast({ message: "claimed · position closed", variant: "success" });
      }
      setTimeout(() => setToast(null), 4500);
    }
    setModal(null);
    // Trigger refetches so the rows reflect the new on-chain state.
    ownedPositions.refetch();
    enriched.refetch();
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
