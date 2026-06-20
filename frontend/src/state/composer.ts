// Composer state.
//
// Fully live: strikes + RFQ quotes come from api-service / quoting-service,
// spot from Pyth, wallet balances from on-chain `getBalance`, and the bucket
// cursor/queue from the `/buckets` response. The hook keeps a single return
// shape so UI components don't change.
import { useEffect, useMemo, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { normalizeStructTag } from "@mysten/sui/utils";
import { useSubmitTransaction } from "../tx/submit";
import { posthog } from "../lib/posthog";
import { useBuckets } from "../api/useBuckets";
import { useCoinBalance } from "../api/useCoinBalance";
import { usePythPrice } from "../api/usePythPrice";
import { useRfq } from "../api/useRfq";
import { useBulkView } from "../api/useBulkView";
import { buildBuyTx, buildWriteTx } from "../tx/composer";
import { formatPrice } from "../format";
import { addSessionTrade } from "../tx/session";
import { useUserIdentity } from "../session/identity";
import { executeWithSession } from "../session/store";
import type { ToastState } from "../components/Toast";
import type { Bucket as ApiBucket, Series } from "../api/client";
import type { RfqQuoteEntry, Side as ProtocolSide } from "../api/quoting";
import type {
  Bucket,
  ConfirmStage,
  ConfirmSummary,
  Quote,
  Strike,
  View,
} from "../types";

// A signed quote carries a short TTL (spec §4.4, ~30–60s). We refuse to
// broadcast one that's already lapsed — or about to, before the tx can land —
// since the on-chain `check_non_signature_fields` would abort with
// E_QUOTE_EXPIRED (code 1). The buffer covers signing + consensus latency.
const QUOTE_EXPIRY_BUFFER_MS = 5000;

function formatExpiry(iso: string): string {
  if (!iso) return "—";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
}

function shortenId(id: string): string {
  const s = id.startsWith("0x") ? id.slice(2) : id;
  if (s.length <= 8) return `0x${s}`;
  return `0x${s.slice(0, 4)}…${s.slice(-4)}`;
}

/** Raw smallest-units → display units. `decimals === null` → unscaled. */
function scaleRaw(raw: string, decimals: number | null): number {
  if (decimals === null) return Number(raw);
  return Number(raw) / 10 ** decimals;
}

/** Format a tile's premium as a whole-dollar amount (no decimals). */
function formatPremium(v: number): string {
  if (!Number.isFinite(v)) return "—";
  if (v <= 0) return "0";
  return Math.round(v).toString();
}

// Default trade size scaled to the asset's price: aim for ~$1k of notional,
// then snap the resulting quantity to a tidy 1/2/5 round number so the field
// pre-fills with e.g. 0.01 BTC, 0.5 ETH, or 500 of a $2 token rather than a
// flat 0.05 that's $5k on one asset and pennies on another.
const DEFAULT_NOTIONAL_USD = 1000;

function niceDefaultAmount(spot: number): number {
  if (!Number.isFinite(spot) || spot <= 0) return 0.05;
  const raw = DEFAULT_NOTIONAL_USD / spot;
  const exp = Math.floor(Math.log10(raw));
  const base = 10 ** exp;
  const frac = raw / base; // in [1, 10)
  const mult = frac < 1.5 ? 1 : frac < 3.5 ? 2 : frac < 7.5 ? 5 : 10;
  return mult * base;
}

/**
 * Map quoting-service `RfqQuoteEntry`s into the UI's `Quote` shape.
 *
 * `mm_reputation` is a 0..1 score from the service; we project to 0..100
 * for the existing "fill" gauge. `revertRate` and `latency` aren't yet
 * surfaced by the service — left at 0 so the gauges read as "no data"
 * rather than misleading values.
 */
function rfqEntriesToUi(
  entries: RfqQuoteEntry[],
  settlementDecimals: number | null,
): Quote[] {
  const now = Date.now();
  const settlementScale = settlementDecimals !== null ? 10 ** settlementDecimals : 1;
  return entries.map((e) => {
    const premium = Number(e.quote.premium) / settlementScale;
    const validUntil = Number(e.quote.valid_until_ms);
    const ttl = Number.isFinite(validUntil)
      ? Math.max(0, Math.round((validUntil - now) / 1000))
      : 0;
    const label = shortenId(e.mm_id);
    return {
      id: `${e.mm_id}-${e.quote.nonce}`,
      name: label,
      addr: label,
      fill: Math.max(0, Math.min(100, Math.round(e.mm_reputation * 100))),
      revertRate: 0,
      latency: 0,
      premium,
      ttl,
      arrivedAt: now,
    };
  });
}

export type ComposerStateOpts = {
  initialView?: View;
  initialAmount?: number;
  initialIdx?: number;
};

export type AssetOption = {
  symbol: string;
  decimals: number | null;
};

export type ExpiryOption = {
  ms: number;
  iso: string;
};

export type ComposerState = {
  view: View;
  setView: (v: View) => void;
  connected: boolean;
  address: string | null;
  spot: number;
  /** True when a feed is expected but no Pyth price has arrived yet. */
  spotUnavailable: boolean;
  amount: number;
  setAmount: (n: number) => void;
  selectedIdx: number;
  setSelectedIdx: (n: number) => void;
  quotes: Quote[];
  bestPremium: number;
  /** True while a firm RFQ quote for the selected strike is in flight. */
  premiumLoading: boolean;
  selected: Strike;
  strikes: Strike[];
  insufficient: boolean;
  insufficientBtc: boolean;
  insufficientUsdc: boolean;
  btcBalance: number;
  usdcBalance: number;
  bucket: Bucket;
  /**
   * Selected bucket exactly as served by `/buckets` (carries
   * `deepbook_pool_id` / `tradeable`, SO-154). Null until a strike is picked.
   */
  apiBucket: ApiBucket | null;
  /** Every strike's bucket in the selected series (parallel to `strikes`),
   * carrying `deepbook_pool_id` so the chain table can fetch each pool. */
  apiBuckets: ApiBucket[];
  confirmStage: ConfirmStage;
  confirmSummary: ConfirmSummary | null;
  submit: () => void;
  closeConfirm: () => void;
  toast: ToastState | null;
  /** Series the strikes come from (asset/settlement/expiry). Null while loading or if none exist. */
  series: Series | null;
  /** True until the first /buckets fetch resolves. */
  bucketsLoading: boolean;
  /** Resolved /buckets fetch returned zero series. */
  bucketsEmpty: boolean;
  /** Distinct assets across all returned series, sorted by symbol. */
  assets: AssetOption[];
  /** Currently selected asset symbol, or null until the first series arrives. */
  selectedAsset: string | null;
  selectAsset: (symbol: string) => void;
  /** Expiries available for `selectedAsset`, sorted ascending by time. */
  expiries: ExpiryOption[];
  /** Currently selected expiry (unix ms), or null until a series is picked. */
  selectedExpiryMs: number | null;
  selectExpiry: (ms: number) => void;
};

export function useComposerState({
  initialView = "writer",
  initialAmount = 0.05,
  initialIdx = 2,
}: ComposerStateOpts = {}): ComposerState {
  const [view, setView] = useState<View>(initialView);
  // Wallet user or session login — both can trade. For sessions, balances
  // live in the options Account custody rather than at an address.
  const identity = useUserIdentity();
  const sessionState = identity?.kind === "session" ? identity.session : null;
  const connected = !!identity;
  const wallet = identity?.address ?? null;
  const [amount, setAmount] = useState(initialAmount);
  const [selectedIdx, setSelectedIdx] = useState(initialIdx);
  const [confirmStage, setConfirmStage] = useState<ConfirmStage>(null);
  const [confirmSummary, setConfirmSummary] = useState<ConfirmSummary | null>(null);
  const [toast, setToast] = useState<ToastState | null>(null);

  // Live strikes: user picks (asset, expiry); we look up the matching series
  // from the api-service response.
  // Expired series can't be invested in, so the picker asks the backend to drop
  // them (`?exclude_expired=true`).
  const bucketsQuery = useBuckets({ excludeExpired: true });
  // SO-69: the writer screen hides invalidated buckets — admin froze them
  // and new writes would revert. Series with no remaining buckets after
  // filtering disappear from the picker too. The trader screen still
  // shows every bucket; users without an existing position can't enter
  // anyway, and the quoting-service rejects RFQs against invalidated
  // buckets with an explicit error.
  const seriesList: Series[] = useMemo(() => {
    // Defensive re-filter of expired series in case the backend served any
    // (clock skew, an older api-service) — the picker must never offer one.
    const now = Date.now();
    const raw = (bucketsQuery.data ?? []).filter((s) => s.expiry_ms > now);
    if (view !== "writer") return raw;
    return raw
      .map((s) => ({ ...s, buckets: s.buckets.filter((b) => !b.invalidated) }))
      .filter((s) => s.buckets.length > 0);
  }, [bucketsQuery.data, view]);

  const assets: AssetOption[] = useMemo(() => {
    const seen = new Map<string, AssetOption>();
    for (const s of seriesList) {
      if (!seen.has(s.asset_symbol)) {
        seen.set(s.asset_symbol, { symbol: s.asset_symbol, decimals: s.asset_decimals });
      }
    }
    return Array.from(seen.values()).sort((a, b) => a.symbol.localeCompare(b.symbol));
  }, [seriesList]);

  const [selectedAsset, setSelectedAsset] = useState<string | null>(null);
  const [selectedExpiryMs, setSelectedExpiryMs] = useState<number | null>(null);

  // Default / clamp selectedAsset when the asset list changes.
  useEffect(() => {
    if (assets.length === 0) {
      if (selectedAsset !== null) setSelectedAsset(null);
      return;
    }
    if (!selectedAsset || !assets.some((a) => a.symbol === selectedAsset)) {
      setSelectedAsset(assets[0].symbol);
    }
  }, [assets, selectedAsset]);

  const expiries: ExpiryOption[] = useMemo(() => {
    if (!selectedAsset) return [];
    return seriesList
      .filter((s) => s.asset_symbol === selectedAsset)
      .map((s) => ({ ms: s.expiry_ms, iso: s.expiry_iso }))
      .sort((a, b) => a.ms - b.ms);
  }, [seriesList, selectedAsset]);

  // Default / clamp selectedExpiryMs whenever the expiry list shifts.
  useEffect(() => {
    if (expiries.length === 0) {
      if (selectedExpiryMs !== null) setSelectedExpiryMs(null);
      return;
    }
    if (selectedExpiryMs === null || !expiries.some((e) => e.ms === selectedExpiryMs)) {
      setSelectedExpiryMs(expiries[0].ms);
    }
  }, [expiries, selectedExpiryMs]);

  const series: Series | null = useMemo(() => {
    if (!selectedAsset || selectedExpiryMs === null) return null;
    return (
      seriesList.find(
        (s) => s.asset_symbol === selectedAsset && s.expiry_ms === selectedExpiryMs,
      ) ?? null
    );
  }, [seriesList, selectedAsset, selectedExpiryMs]);

  // Spot via Pyth, keyed by the selected asset symbol (e.g. "TBTC"). `null`
  // until the first price arrives, or if the symbol has no mapped feed.
  const live = usePythPrice(selectedAsset);
  const spot = live?.price ?? 0;
  const spotUnavailable = selectedAsset !== null && live === null;

  // Scale the pre-filled amount to the asset's price the first time spot lands
  // (~$100 of notional, snapped to a round number). Fires once and only while
  // the field still holds the untouched default, so it never clobbers a user
  // edit made before the price arrived.
  const defaultedAmount = useRef(false);
  useEffect(() => {
    if (defaultedAmount.current || spot <= 0) return;
    defaultedAmount.current = true;
    setAmount((cur) => (cur === initialAmount ? niceDefaultAmount(spot) : cur));
  }, [spot, initialAmount]);

  // Wallet balances from on-chain `getBalance`, scaled by each side's
  // decimals. Resolve coin types from the selected series.
  const underlyingBal = useCoinBalance(
    sessionState ? null : wallet,
    series?.asset_coin_type ?? null,
  );
  const settlementBal = useCoinBalance(
    sessionState ? null : wallet,
    series?.settlement_coin_type ?? null,
  );
  const custodyRaw = (coinType: string | undefined | null): string =>
    coinType
      ? (sessionState?.balances[normalizeStructTag(coinType)] ?? 0n).toString()
      : "0";
  const btcBalance = sessionState
    ? scaleRaw(custodyRaw(series?.asset_coin_type), series?.asset_decimals ?? null)
    : scaleRaw(underlyingBal.data ?? "0", series?.asset_decimals ?? null);
  const usdcBalance = sessionState
    ? scaleRaw(custodyRaw(series?.settlement_coin_type), series?.settlement_decimals ?? null)
    : scaleRaw(settlementBal.data ?? "0", series?.settlement_decimals ?? null);

  const settlementSymbol = series?.settlement_symbol ?? "USDC";

  // Buckets in the selected series with a known strike — the tile axis. Strike
  // order is the api-service's (ascending); tiles, premiums, and selection all
  // index this list so duplicate display-strikes stay distinct.
  const seriesBuckets = useMemo(
    () => (series?.buckets ?? []).filter((b) => b.strike !== null),
    [series],
  );

  // Raw write amount (underlying smallest-units) for the current input. Drives
  // both the signed RFQ and the bulk-view tile premiums; null disables them.
  const writeAmountRaw: string | null = useMemo(() => {
    const dec = series?.asset_decimals;
    if (dec === null || dec === undefined) return null;
    if (!Number.isFinite(amount) || amount <= 0) return null;
    // Convert display-units → raw smallest-units. Safe up to ~2^53 raw
    // (Number's integer precision ceiling) — well above any realistic
    // BTC/SUI/USDC amount the UI lets the user type.
    return Math.round(amount * 10 ** dec).toString();
  }, [series?.asset_decimals, amount]);

  // Indicative per-tile premiums via the bulk-view RFQ: one request covering
  // every strike at the current amount, averaged across opted-in MMs and
  // cached server-side (stale-while-revalidate). Served by Trader MMs on the
  // writer (Earn) side and Writer MMs on the trader (Buy) side.
  const bulkBucketIds = useMemo(
    () => seriesBuckets.map((b) => b.bucket_id),
    [seriesBuckets],
  );
  const { premiums: bulkPremiums } = useBulkView({
    bucketIds: bulkBucketIds,
    writeAmountRaw,
    side: view,
    enabled: !bucketsQuery.isLoading,
  });

  // Real RFQ flow: when the user picks (asset, expiry, strike) and types an
  // amount, send an RFQRequest to the quoting service over WS. The response —
  // already sorted best-price-first for `view` — drives the on-screen quote
  // feed, the headline premium, and the selected tile's firm premium.
  // Selection indexes `seriesBuckets` directly so two strikes that collide on
  // display value still resolve to the right bucket_id.
  const selectedBucketId: string | null = useMemo(
    () => seriesBuckets[selectedIdx]?.bucket_id ?? null,
    [seriesBuckets, selectedIdx],
  );

  const rfqSide: ProtocolSide = view; // View ⊂ Side at the value level.
  const { quotes: rfqEntries, status: rfqStatus, refresh: refreshRfq } = useRfq({
    bucketId: selectedBucketId,
    writeAmountRaw,
    side: rfqSide,
    enabled: !bucketsQuery.isLoading,
  });

  const quotes: Quote[] = useMemo(
    () => rfqEntriesToUi(rfqEntries, series?.settlement_decimals ?? null),
    [rfqEntries, series?.settlement_decimals],
  );

  const bestPremium = quotes[0]?.premium ?? 0;

  // A firm RFQ for the selected strike is in flight — drives the hero-premium
  // wave loader so we show motion instead of a stale "0" while quoting.
  const premiumLoading = rfqStatus === "pending";

  // The signed RFQ for the selected tile returns a firm premium that
  // supersedes its indicative bulk-view average; show it on that tile once it
  // arrives (both Buy and Earn sides). Other tiles keep their bulk-view
  // premium.
  const realSelectedPremium = rfqEntries.length > 0 ? bestPremium : null;

  // Tiles show the indicative bulk-view premium (or the firm RFQ premium for
  // the selected tile), already in settlement display units; placeholder until
  // a premium is available.
  const strikes = useMemo<Strike[]>(() => {
    const scale =
      series?.settlement_decimals != null ? 10 ** series.settlement_decimals : 1;
    return seriesBuckets.map((b) => {
      const entry = bulkPremiums.get(b.bucket_id);
      const indicative = entry ? Number(entry.premium) / scale : null;
      const value =
        (b.bucket_id === selectedBucketId ? realSelectedPremium : null) ??
        indicative;
      return {
        strike: b.strike as number,
        perUnit: 0,
        premium: value ?? 0,
        premiumDisplay: value !== null ? formatPremium(value) : "—",
      };
    });
  }, [
    seriesBuckets,
    bulkPremiums,
    series?.settlement_decimals,
    selectedBucketId,
    realSelectedPremium,
  ]);

  // Clamp selection when the strike list shrinks/grows underneath us
  // (e.g. first /buckets fetch resolves, or a new bucket appears).
  useEffect(() => {
    if (strikes.length === 0) return;
    if (selectedIdx >= strikes.length) setSelectedIdx(strikes.length - 1);
  }, [strikes.length, selectedIdx]);

  // Placeholder used while strikes are loading or empty — keeps downstream
  // components from crashing on `selected.strike` etc. Composer screen
  // gates the interactive UI on bucketsLoading / bucketsEmpty instead.
  const placeholderStrike: Strike = {
    strike: 0,
    perUnit: 0,
    premium: 0,
    premiumDisplay: "—",
  };
  const selected: Strike = strikes[selectedIdx] ?? strikes[0] ?? placeholderStrike;

  const bucketsLoading = bucketsQuery.isLoading;
  const bucketsEmpty = !bucketsLoading && strikes.length === 0;

  // Insufficiency uses real balances. Writer supplies the underlying
  // (`amount`); trader pays the live premium.
  const insufficientBtc = amount > btcBalance;
  const insufficientUsdc = bestPremium > usdcBalance;
  const insufficient = view === "writer" ? insufficientBtc : insufficientUsdc;

  // Tideline state from the selected bucket's live cursor + total_written.
  // `queued` is the amount written ahead of (but not yet exercised before)
  // a new write, which lands at `total_written`. `cap` extends past the new
  // write so its zone stays visible and the bar never divides by zero.
  const bucket: Bucket = useMemo(() => {
    const dec = series?.asset_decimals ?? null;
    const b = series?.buckets.find((x) => x.bucket_id === selectedBucketId) ?? null;
    const cursor = b ? (b.exercise_cursor ?? scaleRaw(b.exercise_cursor_raw, dec)) : 0;
    const totalWritten = b ? (b.total_written ?? scaleRaw(b.total_written_raw, dec)) : 0;
    const queued = Math.max(0, totalWritten - cursor);
    const cap = totalWritten + amount > 0 ? totalWritten + amount : 1;
    return { cursor, queued, cap };
  }, [series, selectedBucketId, amount]);

  // The selected bucket exactly as `/buckets` served it — the trade-venue UI
  // needs its `deepbook_pool_id` / `tradeable` fields verbatim (SO-154).
  const apiBucket: ApiBucket | null = useMemo(
    () => series?.buckets.find((x) => x.bucket_id === selectedBucketId) ?? null,
    [series, selectedBucketId],
  );

  // Safety net (SO-171): the scheduler creates a DeepBook pool for every bucket
  // at deploy time, so a live strike is normally tradeable. If the selected
  // strike is somehow not tradeable (expired/cleaned, or a pool that never
  // landed), surface it as an error toast rather than silently hiding the
  // chart + trade panel. Keyed on primitives so it fires once per strike, not
  // on every /buckets poll.
  const selectedTradeable = apiBucket?.tradeable ?? true;
  useEffect(() => {
    if (view !== "trader" || selectedBucketId == null || selectedTradeable) return;
    setToast({ message: "this strike isn't tradeable right now", variant: "error" });
    const t = setTimeout(() => setToast(null), 5000);
    return () => clearTimeout(t);
  }, [view, selectedBucketId, selectedTradeable]);

  const submitTx = useSubmitTransaction();
  const queryClient = useQueryClient();

  const submit = async () => {
    if (insufficient || !connected || rfqEntries.length === 0 || !series || !wallet)
      return;
    const entry = rfqEntries[0]; // best-price-first

    // Guard against firing a doomed tx: if the chosen quote's TTL has lapsed
    // (or is within the latency buffer of doing so), force a fresh RFQ and
    // bail rather than letting the on-chain expiry check revert.
    const validUntilMs = Number(entry.quote.valid_until_ms);
    if (
      !Number.isFinite(validUntilMs) ||
      validUntilMs - Date.now() <= QUOTE_EXPIRY_BUFFER_MS
    ) {
      refreshRfq();
      setToast({ message: "quote expired · requesting a fresh quote", variant: "info" });
      setTimeout(() => setToast(null), 4000);
      return;
    }

    setConfirmStage("signing");
    try {
      const callCoinType = series.buckets.find(
        (b) => b.bucket_id === entry.quote.bucket_id,
      )?.call_coin_type;
      if (!callCoinType) {
        setConfirmStage(null);
        setToast({ message: "bucket metadata not loaded · try again", variant: "info" });
        setTimeout(() => setToast(null), 4000);
        return;
      }
      setConfirmStage("broadcast");
      if (sessionState) {
        // Session login: custody-funded `_with_session` flow, sponsored and
        // signed by the ephemeral key.
        await executeWithSession(view === "trader" ? "buying" : "writing", (tx, ctx) =>
          addSessionTrade(tx, ctx, {
            entry,
            underlyingCoinType: series.asset_coin_type,
            settlementCoinType: series.settlement_coin_type,
            callCoinType,
            flow: view === "trader" ? "trader" : "writer",
          }),
        );
      } else {
        const tx =
          view === "trader"
            ? buildBuyTx({
                entry,
                underlyingCoinType: series.asset_coin_type,
                settlementCoinType: series.settlement_coin_type,
                callCoinType,
                trader: wallet,
              })
            : buildWriteTx({
                entry,
                underlyingCoinType: series.asset_coin_type,
                settlementCoinType: series.settlement_coin_type,
                callCoinType,
                writer: wallet,
              });
        await submitTx(tx);
      }

      const rangeStart = bucket.cursor + bucket.queued;
      const asset = series.asset_symbol;
      const expiry = formatExpiry(series.expiry_iso);
      setConfirmSummary({
        view,
        premium: bestPremium,
        bucket: `${asset}·${expiry}·${(selected.strike / 1000).toFixed(1)}k`,
        rangeStart,
        rangeEnd: rangeStart + amount,
        amount,
        strike: selected.strike,
        asset,
        expiry,
      });
      setConfirmStage("confirmed");
      posthog.capture(view === "writer" ? "option_written" : "option_purchased", {
        asset: series.asset_symbol,
        strike: selected.strike,
        expiry: series.expiry_iso,
        amount,
        premium: bestPremium,
        settlement_symbol: series.settlement_symbol,
        wallet_address: wallet,
        auth: sessionState ? "session" : "wallet",
      });
      // Reflect the new position on the Dashboard without a manual refresh.
      queryClient.invalidateQueries({ queryKey: ["buckets"] });
      queryClient.invalidateQueries({ queryKey: ["positions", wallet] });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      posthog.captureException(err, {
        action: view === "writer" ? "option_written" : "option_purchased",
        wallet_address: wallet,
        auth: sessionState ? "session" : "wallet",
      });
      setToast({ message: `failed · ${message}`, variant: "error" });
      setTimeout(() => setToast(null), 6000);
      setConfirmStage(null);
    }
  };

  const closeConfirm = () => {
    const s = confirmSummary;
    setConfirmStage(null);
    if (s) {
      setToast({
        message:
          view === "writer"
            ? `position opened · +${formatPrice(s.premium)} ${settlementSymbol} received`
            : `call purchased · ${s.amount.toFixed(4)} ${s.asset} strike $${formatPrice(s.strike, { grouping: true })}`,
        variant: "success",
      });
      setTimeout(() => setToast(null), 4500);
    }
  };

  return {
    view,
    setView,
    connected,
    address: wallet,
    spot,
    spotUnavailable,
    amount,
    setAmount,
    selectedIdx,
    setSelectedIdx,
    quotes,
    bestPremium,
    premiumLoading,
    selected,
    strikes,
    insufficient,
    insufficientBtc,
    insufficientUsdc,
    btcBalance,
    usdcBalance,
    bucket,
    apiBucket,
    apiBuckets: seriesBuckets,
    confirmStage,
    confirmSummary,
    submit,
    closeConfirm,
    toast,
    series,
    bucketsLoading,
    bucketsEmpty,
    assets,
    selectedAsset,
    selectAsset: setSelectedAsset,
    expiries,
    selectedExpiryMs,
    selectExpiry: setSelectedExpiryMs,
  };
}
