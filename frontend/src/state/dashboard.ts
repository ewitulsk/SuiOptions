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
import { useCurrentAccount } from "@mysten/dapp-kit";
import { normalizeStructTag } from "@mysten/sui/utils";
import { useSubmitTransaction } from "../tx/submit";
import { posthog } from "../lib/posthog";

import type { ToastState } from "../components/Toast";
import { useBuckets } from "../api/useBuckets";
import { useOwnedCallOptions } from "../api/useOwnedCallOptions";
import { useOwnedPutOptions } from "../api/useOwnedPutOptions";
import { useCallTokenLots } from "../api/useCallTokenLots";
import { useDashboardPnl } from "../api/useDashboardPnl";
import { useOwnedPositions, type OwnedPositionObj } from "../api/useOwnedPositions";
import { usePythPrices } from "../api/usePythPrice";
import { useBalanceManager, useBmCoinBalances } from "../api/deepbook";
import { fetchEnrichedPositions, optionCoinType, seriesOptionType } from "../api/client";
import type { BucketPnl, CallTokenLot, EnrichedPosition, Position, Series } from "../api/client";
import type { OwnedCallOption } from "../api/useOwnedCallOptions";
import { buildExerciseTx, buildRedeemTx } from "../tx/dashboard";
import { buildExercisePutTx, buildRedeemPutTx } from "../tx/dashboard_put";
import { buildWithdrawBaseTx } from "../tx/deepbook";
import type {
  DashboardModal,
  DashboardSpots,
  DashboardTotals,
  OptionType,
  OwnedLot,
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

const FIFO_EPS = 1e-9;

function buildOwnedRow(
  obj: OwnedCallOption,
  lotsByBucket: Map<string, CallTokenLot[]>,
  pnlByBucket: Map<string, BucketPnl>,
  bucketIdx: Map<string, { series: Series; bucket: Series["buckets"][number] }>,
  spot: number,
  now: number,
  /** Portion of `obj.amount_raw` held in the DeepBook trading account (BM). */
  bmAmountRaw: bigint,
  optionType: OptionType,
): OwnedPosition | null {
  const bucketInfo = bucketIdx.get(obj.bucket_id);
  if (!bucketInfo) return null;
  const { series } = bucketInfo;

  // Provenance headline: option coins are fungible, so we show the most-recent
  // WriteExecuted lot's seller for `boughtFrom`/`boughtAt` (per-lot cost basis
  // comes from the FIFO ledger below, not this list).
  const bucketLots = lotsByBucket.get(obj.bucket_id) ?? [];
  const provenance = bucketLots.length > 0 ? bucketLots[0] : null;

  const amount = scaleU64(obj.amount_raw, series.asset_decimals);
  const strike = series.buckets.find((b) => b.bucket_id === obj.bucket_id)?.strike ?? 0;
  const dte = daysUntil(series.expiry_ms, now);
  // Gate on the real timestamp, not the rounded `dte`: an option that expired
  // up to ~12h ago rounds to dte=0 and would otherwise read as not-expired,
  // leaving a live Exercise button. On-chain `exercise` asserts
  // `timestamp_ms() < expiry_ms` (bucket.move), so anything at/after expiry is
  // unexercisable and must be gated here.
  const expired = now >= series.expiry_ms;
  // PUT is ITM when spot < strike (mirror of the CALL spot > strike); payoff is
  // inverted (strike − spot). Moneyness keeps the same convention (positive =
  // ITM) by flipping its sign for puts.
  const isPut = optionType === "put";
  const itm = spot > 0 && (isPut ? spot < strike : spot > strike);
  const moneyness =
    strike > 0 ? ((isPut ? strike - spot : spot - strike) / strike) * 100 : 0;
  const intrinsicNow = Math.max(
    0,
    (isPut ? strike - spot : spot - strike) * amount,
  );

  // True cost basis (SO-209): start from the backend's FIFO remaining lots
  // (RFQ + DeepBook acquisitions net of tracked sells/exercises/burns), then
  // reconcile against the actual current holding `amount`:
  //   surplus → tokens transferred in off-venue, a $0-cost lot (its own row);
  //   deficit → an untracked transfer-out, consumed FIFO at $0 proceeds (a
  //   realized loss equal to the consumed cost).
  const bucketPnl = pnlByBucket.get(obj.bucket_id);
  const lots: OwnedLot[] = (bucketPnl?.remaining_lots ?? []).map((l) => ({
    amount: l.amount,
    cost: l.cost,
    source: l.source,
    acquiredAtMs: l.acquired_at_ms,
  }));
  let realizedPnl = bucketPnl?.realized_pnl ?? 0;

  const trackedAmount = lots.reduce((sum, l) => sum + l.amount, 0);
  if (amount > trackedAmount + FIFO_EPS) {
    lots.push({ amount: amount - trackedAmount, cost: 0, source: "transfer", acquiredAtMs: 0 });
  } else if (amount < trackedAmount - FIFO_EPS) {
    let deficit = trackedAmount - amount;
    while (deficit > FIFO_EPS && lots.length > 0) {
      const lot = lots[0];
      const take = Math.min(deficit, lot.amount);
      const costTake = lot.amount > FIFO_EPS ? lot.cost * (take / lot.amount) : 0;
      lot.amount -= take;
      lot.cost -= costTake;
      realizedPnl -= costTake; // transfer-out: $0 proceeds → realize the cost
      deficit -= take;
      if (lot.amount <= FIFO_EPS) lots.shift();
    }
  }

  const costBasis = lots.reduce((sum, l) => sum + l.cost, 0);
  const pnl = intrinsicNow - costBasis;
  const totalPnl = realizedPnl + pnl;
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
    optionType,
    asset: displayAsset(series.asset_symbol),
    strike,
    expiry: isoDate(series.expiry_ms),
    amount,
    tradingAccountAmount: scaleU64(bmAmountRaw.toString(), series.asset_decimals),
    premiumPaid: costBasis,
    boughtFrom: provenance ? shortAccount(provenance.seller_account_id) : "—",
    boughtAt: provenance ? isoDate(provenance.timestamp_ms) : "",
    rangeId: shortAccount(obj.bucket_id),
    lots,
    spot,
    dte,
    itm,
    moneyness,
    intrinsicNow,
    pnl,
    realizedPnl,
    totalPnl,
    unpricedExerciseAmount: bucketPnl?.unpriced_exercise_amount ?? 0,
    status,
  };
}

// ── written-row construction ──────────────────────────────────────────

function buildWrittenRow(
  p: Position,
  liveCursor: { totalWrittenRaw: string; cursorRaw: string } | null,
  spot: number,
  now: number,
  optionType: OptionType,
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
    optionType,
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

/** A token balance the user holds in their DeepBook trading account (BM). */
export type TradingAccountBalance = {
  coinType: string;
  symbol: string;
  amount: number;
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
  submit: () => Promise<void>;
  closeModal: () => void;
  /** Settlement tokens the user holds in their DeepBook trading account. */
  tradingAccountSettlements: TradingAccountBalance[];
  /** Withdraw a single position token out of the DeepBook trading account. */
  withdrawFromTradingAccount: (p: OwnedPosition) => void;
  toast: ToastState | null;
  connected: boolean;
  /** Connected wallet address, or null when disconnected. */
  address: string | null;
};

export function useDashboardState(): DashboardState {
  const account = useCurrentAccount();
  const wallet = account?.address ?? null;
  const connected = wallet !== null;

  // Written positions: the wallet is the authoritative list (transfer-correct,
  // no projection snapshot bound), enriched by object id via api-service.
  const ownedPositions = useOwnedPositions(wallet);
  const buckets = useBuckets();
  // Option coins live in per-roll packages; the bucket list supplies the
  // coin-type→bucket mapping the owned-call query needs.
  const owned = useOwnedCallOptions(wallet, buckets.data);
  // Cash-secured PUT holdings (`Coin<Put>`), mapped to their put buckets.
  const ownedPuts = useOwnedPutOptions(wallet, buckets.data);
  // Per-purchase provenance for owned calls (boughtFrom / premiumPaid / boughtAt).
  const lots = useCallTokenLots(wallet);

  // DeepBook trading account (BM): position tokens and settlement the user left
  // sitting in their balance manager rather than the wallet.
  const bmId = useBalanceManager(wallet);
  // True cost-basis PnL (FIFO lots + realized), attributed to the wallet's BM.
  const pnl = useDashboardPnl(wallet, bmId.data ?? null);
  const bmQueryTypes = useMemo<string[]>(() => {
    const set = new Set<string>();
    for (const s of buckets.data ?? []) {
      if (s.settlement_coin_type) set.add(s.settlement_coin_type);
      // Cover both call and put option coin types (option_coin_type ?? call_coin_type).
      for (const b of s.buckets) {
        const ct = optionCoinType(b);
        if (ct) set.add(ct);
      }
    }
    return Array.from(set);
  }, [buckets.data]);
  const bmBalances = useBmCoinBalances(bmId.data ?? null, bmQueryTypes, wallet);

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
    // FIFO cost-basis ledger per bucket (SO-209).
    const pnlByBucket = new Map<string, BucketPnl>();
    for (const b of pnl.data ?? []) pnlByBucket.set(b.bucket_id, b);

    // Normalized option-coin type → { bucket id, option type }. Covers both
    // covered-call and cash-secured-put buckets (puts read option_coin_type).
    const optionTypeMeta = new Map<string, { bucketId: string; optionType: OptionType }>();
    for (const s of buckets.data ?? []) {
      const ot = seriesOptionType(s);
      for (const b of s.buckets) {
        const ct = optionCoinType(b);
        if (ct) optionTypeMeta.set(normalizeStructTag(ct), { bucketId: b.bucket_id, optionType: ot });
      }
    }

    // A holding is wallet balance + trading-account (BM) balance. Union the two
    // so a position that lives *only* in the BM (e.g. a market buy that never
    // got withdrawn) still shows up. Calls and puts both contribute here; the
    // option-type map disambiguates which builder semantics each coin gets.
    const walletByType = new Map<string, bigint>();
    for (const o of [...(owned.data ?? []), ...(ownedPuts.data ?? [])]) {
      walletByType.set(normalizeStructTag(o.coin_type), BigInt(o.amount_raw));
    }
    const bmByType = bmBalances.data ?? {};
    const allTypes = new Set<string>([...walletByType.keys(), ...Object.keys(bmByType)]);

    const rows: OwnedPosition[] = [];
    for (const type of allTypes) {
      const meta = optionTypeMeta.get(type);
      if (!meta) continue; // settlement type, or unknown — not a position
      const { bucketId: bucket_id, optionType } = meta;
      const walletRaw = walletByType.get(type) ?? 0n;
      const bmRaw = bmByType[type] ?? 0n;
      const total = walletRaw + bmRaw;
      if (total <= 0n) continue;
      const series = bucketIdx.get(bucket_id)?.series;
      const symbol = displayAsset(series?.asset_symbol ?? "");
      const spot = spots[symbol] ?? 0;
      const row = buildOwnedRow(
        { coin_type: type, bucket_id, amount_raw: total.toString() },
        lotsByBucket,
        pnlByBucket,
        bucketIdx,
        spot,
        now,
        bmRaw,
        optionType,
      );
      if (row) rows.push(row);
    }
    return rows;
  }, [owned.data, ownedPuts.data, bmBalances.data, buckets.data, lots.data, pnl.data, spots, now]);

  // Settlement balances sitting in the trading account (BM), for the dashboard
  // "in DeepBook" section — one entry per distinct settlement token held.
  const tradingAccountSettlements = useMemo<TradingAccountBalance[]>(() => {
    const bm = bmBalances.data ?? {};
    const meta = new Map<string, { symbol: string; decimals: number | null }>();
    for (const s of buckets.data ?? []) {
      if (s.settlement_coin_type) {
        meta.set(normalizeStructTag(s.settlement_coin_type), {
          symbol: s.settlement_symbol,
          decimals: s.settlement_decimals,
        });
      }
    }
    const out: TradingAccountBalance[] = [];
    for (const [type, m] of meta) {
      const raw = bm[type] ?? 0n;
      if (raw <= 0n) continue;
      out.push({ coinType: type, symbol: m.symbol, amount: scaleU64(raw.toString(), m.decimals) });
    }
    return out;
  }, [bmBalances.data, buckets.data]);

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
      const series = bucketIdx.get(o.bucket_id)?.series;
      const optionType: OptionType = series ? seriesOptionType(series) : "call";
      return buildWrittenRow(pos, liveCursor, spot, now, optionType);
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
    const ownedRealized = ownedRows.reduce((sum, p) => sum + p.realizedPnl, 0);
    const ownedTotalPnl = ownedRows.reduce((sum, p) => sum + p.totalPnl, 0);
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
      ownedRealized,
      ownedTotalPnl,
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
        const isPut = p.optionType === "put";
        // Resolve the bucket by the position's coin type — a BM-only holding
        // has no `owned.data` (wallet) entry, so we can't key off that. Match on
        // the call/put-agnostic option coin type so puts resolve too.
        const norm = normalizeStructTag(p.id);
        const bucketInfo = (buckets.data ?? [])
          .flatMap((s) => s.buckets.map((b) => ({ series: s, b })))
          .find((x) => normalizeStructTag(optionCoinType(x.b)) === norm);
        if (!bucketInfo) {
          throw new Error("missing on-chain reference for exercise");
        }
        const { series, b: bucket } = bucketInfo;
        const optCoinType = optionCoinType(bucket);
        const assetDec = series.asset_decimals ?? 0;
        const exerciseAmountRaw = BigInt(
          Math.round(qty * 10 ** assetDec).toString(),
        );
        // CALL: settlement_raw = exerciseAmount_raw * strike_raw / 10^strike_scale
        // (strike_raw / 10^strike_scale is settlement-smallest per
        // underlying-smallest; multiplying by the raw amount gives raw
        // settlement.) PUT: the holder delivers `exerciseAmountRaw` underlying
        // and receives settlement, so no settlement payment is computed here.
        const strikeRaw = BigInt(bucket.strike_raw);
        const scale = BigInt(10) ** BigInt(bucket.strike_scale);
        const settlementAmountRaw = (exerciseAmountRaw * strikeRaw) / scale;

        setModal({ ...captured, stage: "broadcast" } as DashboardModal);
        // If part/all of the option coin is in the DeepBook trading account,
        // withdraw it inline (atomic) so the exercise can consume it.
        const walletRaw = BigInt(
          ([...(owned.data ?? []), ...(ownedPuts.data ?? [])].find(
            (o) => normalizeStructTag(o.coin_type) === norm,
          )?.amount_raw) ?? "0",
        );
        const bmRaw = (bmBalances.data ?? {})[norm] ?? 0n;
        const bmWithdraw =
          bmRaw > 0n && bmId.data && bucket.deepbook_pool_id
            ? { poolId: bucket.deepbook_pool_id, bmId: bmId.data, walletAmountRaw: walletRaw }
            : undefined;
        const tx = isPut
          ? buildExercisePutTx({
              bucketId: bucket.bucket_id,
              putCoinType: optCoinType,
              exerciseAmountRaw,
              underlyingDeliveryRaw: exerciseAmountRaw,
              underlyingCoinType: series.asset_coin_type,
              settlementCoinType: series.settlement_coin_type,
              recipient: wallet,
              bmWithdraw,
            })
          : buildExerciseTx({
              bucketId: bucket.bucket_id,
              callCoinType: optCoinType,
              exerciseAmountRaw,
              settlementAmountRaw,
              underlyingCoinType: series.asset_coin_type,
              settlementCoinType: series.settlement_coin_type,
              recipient: wallet,
              bmWithdraw,
            });
        await submitTx(tx);
        setModal({ ...captured, stage: "confirmed" } as DashboardModal);
        posthog.capture("option_exercised", {
          asset: captured.position.asset,
          option_type: captured.position.optionType,
          strike: captured.position.strike,
          expiry: captured.position.expiry,
          qty,
          pnl: captured.position.pnl,
          wallet_address: wallet,
          auth: "wallet",
        });
      } else if (captured.kind === "claim") {
        const { position: p } = captured;
        const isPut = p.optionType === "put";
        // Resolve the on-chain object from the wallet (authoritative) and the
        // coin types from /buckets — no dependency on the /positions endpoint.
        const matchPos = (ownedPositions.data ?? []).find((pp) => pp.object_id === p.id);
        const bucketInfo = (buckets.data ?? [])
          .flatMap((s) => s.buckets.map((b) => ({ series: s, b })))
          .find((x) => x.b.bucket_id === matchPos?.bucket_id);
        if (!matchPos || !bucketInfo) {
          throw new Error("missing on-chain reference for claim");
        }
        const optCoinType = optionCoinType(bucketInfo.b);
        setModal({ ...captured, stage: "broadcast" } as DashboardModal);
        const tx = isPut
          ? buildRedeemPutTx({
              bucketId: matchPos.bucket_id,
              positionObjectId: matchPos.object_id,
              putCoinType: optCoinType,
              underlyingCoinType: bucketInfo.series.asset_coin_type,
              settlementCoinType: bucketInfo.series.settlement_coin_type,
              recipient: wallet,
            })
          : buildRedeemTx({
              bucketId: matchPos.bucket_id,
              positionObjectId: matchPos.object_id,
              callCoinType: optCoinType,
              underlyingCoinType: bucketInfo.series.asset_coin_type,
              settlementCoinType: bucketInfo.series.settlement_coin_type,
              recipient: wallet,
            });
        await submitTx(tx);
        setModal({ ...captured, stage: "confirmed" } as DashboardModal);
        posthog.capture("position_claimed", {
          asset: captured.position.asset,
          option_type: captured.position.optionType,
          strike: captured.position.strike,
          expiry: captured.position.expiry,
          premium_received: captured.position.premiumReceived,
          exercised_pct: captured.position.exercisedPct,
          wallet_address: wallet,
          auth: "wallet",
        });
      }
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      posthog.captureException(err, {
        action: captured?.kind,
        wallet_address: wallet,
        auth: "wallet",
      });
      setToast({ message: `failed · ${message}`, variant: "error" });
      setTimeout(() => setToast(null), 6000);
      setModal(null);
    }
  };

  // Standalone "withdraw this position token from DeepBook" — settles the pool
  // and drains only the option coin back to the wallet (leaves settlement in
  // the BM for other trades).
  const withdrawFromTradingAccount = async (p: OwnedPosition) => {
    if (!wallet || !bmId.data) return;
    const norm = normalizeStructTag(p.id);
    const info = (buckets.data ?? [])
      .flatMap((s) => s.buckets.map((b) => ({ series: s, b })))
      .find((x) => normalizeStructTag(optionCoinType(x.b)) === norm);
    if (!info || !info.b.deepbook_pool_id) {
      setToast({ message: "failed · no DeepBook pool for this position", variant: "error" });
      setTimeout(() => setToast(null), 6000);
      return;
    }
    try {
      const tx = buildWithdrawBaseTx({
        poolId: info.b.deepbook_pool_id,
        bmId: bmId.data,
        baseCoinType: optionCoinType(info.b),
        quoteCoinType: info.series.settlement_coin_type,
        recipient: wallet,
      });
      await submitTx(tx);
      setToast({
        message: `withdrew ${p.tradingAccountAmount.toFixed(p.asset === "BTC" ? 4 : 0)} ${p.asset} call from DeepBook`,
        variant: "success",
      });
      setTimeout(() => setToast(null), 4500);
      posthog.capture("deepbook_position_withdrawn", {
        asset: p.asset,
        amount: p.tradingAccountAmount,
        pool_id: info.b.deepbook_pool_id,
        wallet_address: wallet,
      });
      owned.refetch();
      ownedPuts.refetch();
      bmBalances.refetch();
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      posthog.captureException(err, { action: "deepbook_withdraw", wallet_address: wallet });
      setToast({ message: `failed · ${message}`, variant: "error" });
      setTimeout(() => setToast(null), 6000);
    }
  };

  const closeModal = () => {
    if (modal && (modal as { stage?: string }).stage === "confirmed") {
      const k = modal.kind;
      if (k === "exercise") {
        const p = modal.position;
        setToast({
          // PUT exercise delivers the underlying and pays out settlement; CALL
          // exercise pays settlement and receives the underlying.
          message:
            p.optionType === "put"
              ? `exercised · delivered ${modal.qty.toFixed(p.asset === "BTC" ? 4 : 0)} ${p.asset} for ${p.strike} USDC each`
              : `exercised · received ${modal.qty.toFixed(p.asset === "BTC" ? 4 : 0)} ${p.asset}`,
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
    ownedPuts.refetch();
    bmBalances.refetch();
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
    tradingAccountSettlements,
    withdrawFromTradingAccount,
    toast,
    connected,
    address: wallet,
  };
}
