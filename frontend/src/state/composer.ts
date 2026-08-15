// Composer state.
//
// Fully live: strikes + RFQ quotes come from api-service / quoting-service,
// spot from Pyth, wallet balances from on-chain `getBalance`, and the bucket
// cursor/queue from the `/buckets` response. The hook keeps a single return
// shape so UI components don't change.
import { useEffect, useMemo, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useCurrentAccount } from "@mysten/dapp-kit";
import { useSubmitTransaction } from "../tx/submit";
import { posthog } from "../lib/posthog";
import { useBuckets } from "../api/useBuckets";
import { useCoinBalance } from "../api/useCoinBalance";
import { usePythPrice } from "../api/usePythPrice";
import { resolveFeedId } from "../api/pyth";
import { useRfq } from "../api/useRfq";
import { useBulkView } from "../api/useBulkView";
import { buildBuyTx, buildWriteTx } from "../tx/composer";
import { buildBuyPutTx, buildWritePutTx } from "../tx/composer_put";
import { formatPrice } from "../format";
import {
  fetchBucketSpec,
  isUncreated,
  optionCoinType,
  rfqTradeable,
  seriesOptionType,
} from "../api/client";
import {
  buildCreateBucketTx,
  normalizeStrike,
  strikeDisplayToRaw,
} from "../tx/anystrike";
import { BUCKET_REGISTRY_ID } from "../config";
import type { ToastState } from "../components/Toast";
import type { Bucket as ApiBucket, Series } from "../api/client";
import type { RfqQuoteEntry, Side as ProtocolSide } from "../api/quoting";
import type {
  Bucket,
  ConfirmStage,
  ConfirmSummary,
  OptionType,
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
  initialOptionType?: OptionType;
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
  /** Covered CALL vs cash-secured PUT — orthogonal to `view`. */
  optionType: OptionType;
  setOptionType: (t: OptionType) => void;
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
  /** USDC cash collateral a put writer must post for the current amount/strike
   *  (`amount × strike`); 0 for calls. Drives the put-side insufficiency copy. */
  putCollateral: number;
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
  /** Any-strike creation (SO-395): free strike input + sponsored create. */
  customStrike: string;
  setCustomStrike: (v: string) => void;
  /** Non-null while a create tx is in flight / awaiting the indexer. */
  creatingBucket: boolean;
  /** Whether this deployment supports any-strike creation. */
  canCreateStrikes: boolean;
  createCustomStrike: () => Promise<void>;
  /**
   * The selected tile is a strike the board *lists* but which has no bucket
   * yet (SO-400) — there is nothing to quote against until it's created.
   */
  selectedUncreated: boolean;
  /** Create the selected listed strike, so it can then be written. */
  createSelectedStrike: () => Promise<void>;
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
  initialOptionType = "call",
  initialAmount = 0.05,
  initialIdx = 2,
}: ComposerStateOpts = {}): ComposerState {
  const [view, setView] = useState<View>(initialView);
  const [optionType, setOptionType] = useState<OptionType>(initialOptionType);
  const account = useCurrentAccount();
  const connected = !!account;
  const wallet = account?.address ?? null;
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
    const raw = (bucketsQuery.data ?? [])
      .filter((s) => s.expiry_ms > now)
      // Show only the series matching the selected option type (call vs put).
      // Series without an `option_type` are treated as calls.
      .filter((s) => seriesOptionType(s) === optionType);
    if (view !== "writer") {
      // Trader (Buy) side: a strike the board merely *lists* (SO-400) has no
      // bucket, so no option coins exist and there is nothing to buy. Only
      // the writer side can bring one into existence.
      return raw
        .map((s) => ({ ...s, buckets: s.buckets.filter((b) => !isUncreated(b)) }))
        .filter((s) => s.buckets.length > 0);
    }
    return raw
      .map((s) => ({ ...s, buckets: s.buckets.filter((b) => !b.invalidated) }))
      .filter((s) => s.buckets.length > 0);
  }, [bucketsQuery.data, view, optionType]);

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

  // Scale the pre-filled amount to the asset's price (~$1k of notional, snapped
  // to a round number) once spot lands for the selected asset, and again each
  // time the user switches assets. Guarded so it fires once per asset rather
  // than on every live price tick — edits made while staying on an asset stick.
  //
  // `live` lags one render behind a switch (its price updates in an effect), so
  // gate on the feed id actually matching the selected asset — otherwise the
  // first post-switch render would default off the *previous* asset's price and
  // then mark the new asset done, leaving the amount stuck.
  const defaultedFor = useRef<string | null>(null);
  useEffect(() => {
    if (!selectedAsset || !live || spot <= 0) return;
    if (live.feedId !== resolveFeedId(selectedAsset)) return;
    if (defaultedFor.current === selectedAsset) return;
    defaultedFor.current = selectedAsset;
    setAmount(niceDefaultAmount(spot));
  }, [selectedAsset, live, spot]);

  // Wallet balances from on-chain `getBalance`, scaled by each side's
  // decimals. Resolve coin types from the selected series.
  const underlyingBal = useCoinBalance(wallet, series?.asset_coin_type ?? null);
  const settlementBal = useCoinBalance(wallet, series?.settlement_coin_type ?? null);
  const btcBalance = scaleRaw(underlyingBal.data ?? "0", series?.asset_decimals ?? null);
  const usdcBalance = scaleRaw(settlementBal.data ?? "0", series?.settlement_decimals ?? null);

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
  // Only created buckets can be quoted: the bulk view prices real objects, so
  // the listed-but-uncreated strikes (SO-400) are filtered out here and simply
  // show no indicative premium until someone creates them.
  const bulkBucketIds = useMemo(
    () => seriesBuckets.map((b) => b.bucket_id).filter((id): id is string => id !== null),
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
      const entry = b.bucket_id ? bulkPremiums.get(b.bucket_id) : undefined;
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

  // ── Any-strike creation (SO-395) ─────────────────────────────────────
  // Two-step v1: sponsored create-bucket PTB, then the 5s /buckets poll
  // surfaces the new bucket and we auto-select it; RFQ + write proceed as
  // for any other strike. (Single-tx create+quote+write is SO-396.)
  const [customStrike, setCustomStrike] = useState("");
  const [creatingBucket, setCreatingBucket] = useState(false);
  const pendingSpec = useRef<{ sig: string; exp: number; expiryMs: number } | null>(null);
  const canCreateStrikes = Boolean(BUCKET_REGISTRY_ID);

  // Auto-select the created bucket once the poll surfaces it.
  useEffect(() => {
    const pending = pendingSpec.current;
    if (!pending || !series || series.expiry_ms !== pending.expiryMs) return;
    const idx = seriesBuckets.findIndex((b) => {
      // The board lists this strike before it exists (SO-400), so match only
      // *created* buckets — otherwise the synthetic tile would satisfy the
      // wait immediately and we'd report success before the tx landed.
      if (isUncreated(b)) return false;
      const n = normalizeStrike(BigInt(b.strike_raw), b.strike_scale);
      return n !== null && n.sig.toString() === pending.sig && n.exp === pending.exp;
    });
    if (idx >= 0) {
      pendingSpec.current = null;
      setCreatingBucket(false);
      setSelectedIdx(idx);
      setToast({ message: "strike created · quotes incoming", variant: "info" });
      setTimeout(() => setToast(null), 4000);
    }
  }, [seriesBuckets, series]);

  const createCustomStrike = async () => {
    if (!series || creatingBucket) return;
    const underDec = series.asset_decimals;
    const settleDec = series.settlement_decimals;
    if (underDec == null || settleDec == null) {
      setToast({ message: "token decimals unknown for this pair", variant: "error" });
      setTimeout(() => setToast(null), 4000);
      return;
    }
    const raw = strikeDisplayToRaw(customStrike, underDec, settleDec);
    const norm = raw && normalizeStrike(raw.strikeRaw, raw.strikeScale);
    if (!raw || !norm) {
      setToast({
        message: "enter a valid strike (max 13 significant digits)",
        variant: "error",
      });
      setTimeout(() => setToast(null), 4000);
      return;
    }
    await createStrike(raw, norm, underDec);
  };

  /**
   * Bring the *selected* listed strike into existence (SO-400). The board
   * advertises the ladder around spot, so the strike the user picked may not
   * have a bucket yet; this runs the same sponsored create the free-text
   * input does, reusing the strike already encoded in the tile.
   */
  const createSelectedStrike = async () => {
    if (!series || creatingBucket) return;
    const b = seriesBuckets[selectedIdx];
    if (!b || !isUncreated(b)) return;
    const underDec = series.asset_decimals;
    if (underDec == null) {
      setToast({ message: "token decimals unknown for this pair", variant: "error" });
      setTimeout(() => setToast(null), 4000);
      return;
    }
    const strikeRaw = BigInt(b.strike_raw);
    const norm = normalizeStrike(strikeRaw, b.strike_scale);
    if (!norm) return;
    await createStrike({ strikeRaw, strikeScale: b.strike_scale }, norm, underDec);
  };

  /** Shared create path for both the free-text input and a listed tile. */
  const createStrike = async (
    raw: { strikeRaw: bigint; strikeScale: number },
    norm: { sig: bigint; exp: number },
    underDec: number,
  ) => {
    if (!series) return;
    // Already *created*? Select it instead of creating a second time.
    // Uncreated board strikes (SO-400) are deliberately excluded: they share
    // the strike we're about to create, so matching them would short-circuit
    // straight back to the tile and the bucket would never come into being.
    const existingIdx = seriesBuckets.findIndex((b) => {
      if (isUncreated(b)) return false;
      const n = normalizeStrike(BigInt(b.strike_raw), b.strike_scale);
      return n !== null && n.sig === norm.sig && n.exp === norm.exp;
    });
    if (existingIdx >= 0) {
      setSelectedIdx(existingIdx);
      setCustomStrike("");
      return;
    }
    if (!canCreateStrikes) {
      setToast({
        message: "this deployment doesn't support custom strikes yet",
        variant: "error",
      });
      setTimeout(() => setToast(null), 4000);
      return;
    }
    setCreatingBucket(true);
    try {
      // Cross-check against the api-service in case the local list is
      // stale (someone else created this strike since the last poll).
      const spec = await fetchBucketSpec({
        underlying: series.asset_coin_type,
        settlement: series.settlement_coin_type,
        expiryMs: series.expiry_ms,
        strikeRaw: raw.strikeRaw.toString(),
        strikeScale: raw.strikeScale,
        optionType,
      }).catch(() => null);
      if (spec?.exists) {
        pendingSpec.current = {
          sig: norm.sig.toString(),
          exp: norm.exp,
          expiryMs: series.expiry_ms,
        };
        await bucketsQuery.refetch();
        return;
      }
      const tx = buildCreateBucketTx({
        underlyingCoinType: series.asset_coin_type,
        settlementCoinType: series.settlement_coin_type,
        expiryMs: series.expiry_ms,
        strikeRaw: raw.strikeRaw,
        strikeScale: raw.strikeScale,
        coinDecimals: underDec,
        isPut: optionType === "put",
      });
      const digest = await submitTx(tx);
      posthog.capture("bucket_create_submitted", {
        digest,
        // Normalized, so both entry points report the same strike (the
        // free-text field is empty when a listed tile drove the create).
        strike: `${norm.sig}e-${norm.exp}`,
        expiry_ms: series.expiry_ms,
        option_type: optionType,
      });
      pendingSpec.current = {
        sig: norm.sig.toString(),
        exp: norm.exp,
        expiryMs: series.expiry_ms,
      };
      setCustomStrike("");
      setToast({ message: "creating strike on-chain…", variant: "info" });
      setTimeout(() => setToast(null), 4000);
    } catch (e) {
      setCreatingBucket(false);
      const msg = e instanceof Error ? e.message : String(e);
      setToast({ message: msg, variant: "error" });
      setTimeout(() => setToast(null), 6000);
    }
  };

  // Insufficiency uses real balances. A CALL writer supplies the underlying
  // (`amount`); a PUT writer supplies settlement cash collateral
  // (≈ amount * strike). Every buyer (call or put) pays the live premium.
  const putCollateral =
    optionType === "put" && selected.strike > 0 ? amount * selected.strike : 0;
  const insufficientBtc =
    optionType === "put"
      ? putCollateral > usdcBalance // put writer posts settlement collateral
      : amount > btcBalance;
  const insufficientUsdc = bestPremium > usdcBalance;
  const insufficient = view === "writer" ? insufficientBtc : insufficientUsdc;

  // QueueWave state from the selected bucket's live cursor + total_written.
  // `queued` is the amount written ahead of (but not yet exercised before)
  // a new write, which lands at `total_written`. `cap` extends past the new
  // write so its zone stays visible and the bar never divides by zero.
  const bucket: Bucket = useMemo(() => {
    const dec = series?.asset_decimals ?? null;
    // Index the tile array rather than matching on `bucket_id`: an uncreated
    // board strike (SO-400) has a null id, so a `=== selectedBucketId` match
    // would latch onto whichever synthetic bucket happens to come first.
    const b = seriesBuckets[selectedIdx] ?? null;
    const cursor = b ? (b.exercise_cursor ?? scaleRaw(b.exercise_cursor_raw, dec)) : 0;
    const totalWritten = b ? (b.total_written ?? scaleRaw(b.total_written_raw, dec)) : 0;
    const queued = Math.max(0, totalWritten - cursor);
    const cap = totalWritten + amount > 0 ? totalWritten + amount : 1;
    return { cursor, queued, cap };
  }, [series, seriesBuckets, selectedIdx, amount]);

  // The selected bucket exactly as `/buckets` served it — the trade-venue UI
  // needs its `deepbook_pool_id` / `tradeable` fields verbatim (SO-154).
  const apiBucket: ApiBucket | null = useMemo(
    () => seriesBuckets[selectedIdx] ?? null,
    [seriesBuckets, selectedIdx],
  );

  /** The selected strike is listed but has no bucket on chain yet. */
  const selectedUncreated = apiBucket !== null && isUncreated(apiBucket);

  // Safety net (SO-171): if the selected strike isn't tradeable
  // (expired/cleaned, or a pool that never landed), surface it as an error
  // toast rather than silently hiding the chart + trade panel. Keyed on
  // primitives so it fires once per strike, not on every /buckets poll.
  const selectedTradeable = apiBucket ? rfqTradeable(apiBucket) : true;
  useEffect(() => {
    if (view !== "trader" || selectedBucketId == null || selectedTradeable) return;
    setToast({ message: "this strike isn't quotable right now", variant: "error" });
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
      const quoteBucket = series.buckets.find(
        (b) => b.bucket_id === entry.quote.bucket_id,
      );
      const coinType = quoteBucket ? optionCoinType(quoteBucket) : undefined;
      if (!quoteBucket || !coinType) {
        setConfirmStage(null);
        setToast({ message: "bucket metadata not loaded · try again", variant: "info" });
        setTimeout(() => setToast(null), 4000);
        return;
      }
      setConfirmStage("broadcast");
      const isPut = optionType === "put";
      let tx;
      if (isPut) {
        tx =
          view === "trader"
            ? buildBuyPutTx({
                entry,
                underlyingCoinType: series.asset_coin_type,
                settlementCoinType: series.settlement_coin_type,
                putCoinType: coinType,
                trader: wallet,
              })
            : buildWritePutTx({
                entry,
                underlyingCoinType: series.asset_coin_type,
                settlementCoinType: series.settlement_coin_type,
                putCoinType: coinType,
                strikeRaw: quoteBucket.strike_raw,
                strikeScale: quoteBucket.strike_scale,
                writer: wallet,
              });
      } else {
        tx =
          view === "trader"
            ? buildBuyTx({
                entry,
                underlyingCoinType: series.asset_coin_type,
                settlementCoinType: series.settlement_coin_type,
                callCoinType: coinType,
                trader: wallet,
              })
            : buildWriteTx({
                entry,
                underlyingCoinType: series.asset_coin_type,
                settlementCoinType: series.settlement_coin_type,
                callCoinType: coinType,
                writer: wallet,
              });
      }
      await submitTx(tx);

      const rangeStart = bucket.cursor + bucket.queued;
      const asset = series.asset_symbol;
      const expiry = formatExpiry(series.expiry_iso);
      setConfirmSummary({
        view,
        optionType,
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
        option_type: optionType,
        strike: selected.strike,
        expiry: series.expiry_iso,
        amount,
        premium: bestPremium,
        settlement_symbol: series.settlement_symbol,
        wallet_address: wallet,
        auth: "wallet",
      });
      // Reflect the new position on the Dashboard without a manual refresh.
      queryClient.invalidateQueries({ queryKey: ["buckets"] });
      queryClient.invalidateQueries({ queryKey: ["positions", wallet] });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      posthog.captureException(err, {
        action: view === "writer" ? "option_written" : "option_purchased",
        wallet_address: wallet,
        auth: "wallet",
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
            : `${s.optionType === "put" ? "put" : "call"} purchased · ${s.amount.toFixed(4)} ${s.asset} strike $${formatPrice(s.strike, { grouping: true })}`,
        variant: "success",
      });
      setTimeout(() => setToast(null), 4500);
    }
  };

  return {
    view,
    setView,
    optionType,
    setOptionType,
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
    putCollateral,
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
    customStrike,
    setCustomStrike,
    creatingBucket,
    canCreateStrikes,
    createCustomStrike,
    selectedUncreated,
    createSelectedStrike,
    assets,
    selectedAsset,
    selectAsset: setSelectedAsset,
    expiries,
    selectedExpiryMs,
    selectExpiry: setSelectedExpiryMs,
  };
}
