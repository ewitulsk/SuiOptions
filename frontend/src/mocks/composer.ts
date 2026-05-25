// Composer state.
//
// Strikes and quotes are live (api-service /buckets + quoting service RFQs).
// Wallet balances and spot price are still mocked pending real wiring.
import { useEffect, useMemo, useState } from "react";
import { useCurrentAccount } from "@mysten/dapp-kit";
import { useBuckets } from "../api/useBuckets";
import { useRfqQuotes } from "../api/useRfqQuotes";
import type { Series } from "../api/client";
import type {
  Bucket,
  ConfirmStage,
  ConfirmSummary,
  Quote,
  Strike,
  View,
} from "../types";

function formatExpiry(iso: string): string {
  if (!iso) return "—";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
}

function shortId(hex: string): string {
  if (hex.length <= 12) return hex;
  return `${hex.slice(0, 6)}...${hex.slice(-4)}`;
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
  amount: number;
  setAmount: (n: number) => void;
  selectedIdx: number;
  setSelectedIdx: (n: number) => void;
  quotes: Quote[];
  bestPremium: number;
  selected: Strike;
  strikes: Strike[];
  insufficient: boolean;
  insufficientBtc: boolean;
  insufficientUsdc: boolean;
  btcBalance: number;
  usdcBalance: number;
  bucket: Bucket;
  confirmStage: ConfirmStage;
  confirmSummary: ConfirmSummary | null;
  submit: () => void;
  closeConfirm: () => void;
  toast: string | null;
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
  const account = useCurrentAccount();
  const connected = !!account;
  const [spot, setSpot] = useState(79083.44);
  const [amount, setAmount] = useState(initialAmount);
  const [selectedIdx, setSelectedIdx] = useState(initialIdx);
  const [quotes, setQuotes] = useState<Quote[]>([]);
  const [confirmStage, setConfirmStage] = useState<ConfirmStage>(null);
  const [confirmSummary, setConfirmSummary] = useState<ConfirmSummary | null>(null);
  const [toast, setToast] = useState<string | null>(null);

  const [bucket] = useState<Bucket>({ cursor: 0.84, queued: 0.42, cap: 3.0 });

  const btcBalance = 0.4321;
  const usdcBalance = 5000.0;

  // Tick spot
  useEffect(() => {
    const t = setInterval(() => {
      setSpot((s) => +(s + (Math.random() - 0.5) * s * 0.001).toFixed(2));
    }, 4000);
    return () => clearInterval(t);
  }, []);

  // Live strikes: user picks (asset, expiry); we look up the matching series
  // from the api-service response.
  const bucketsQuery = useBuckets();
  const seriesList: Series[] = useMemo(() => bucketsQuery.data ?? [], [bucketsQuery.data]);

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

  const settlementSymbol = series?.settlement_symbol ?? "USDC";
  const liveStrikes: number[] = useMemo(
    () =>
      (series?.buckets ?? [])
        .map((b) => b.strike)
        .filter((s): s is number => s !== null),
    [series],
  );

  const bucketsLoading = bucketsQuery.isLoading;
  const bucketsEmpty = !bucketsLoading && liveStrikes.length === 0;

  // Real RFQ quotes from the quoting service (1-min polling interval).
  const rfqSide = view === "writer" ? "writer" as const : "trader" as const;
  const { quotesByBucket, connected: rfqConnected } = useRfqQuotes({
    series,
    side: rfqSide,
    enabled: !bucketsEmpty && !bucketsLoading,
  });

  const strikes = useMemo<Strike[]>(
    () =>
      liveStrikes.map((strike, idx) => {
        // If we have a real quote for this bucket, use its premium.
        const bucketId = series?.buckets[idx]?.bucket_id;
        const realQuote = bucketId ? quotesByBucket[bucketId] : undefined;
        if (realQuote && realQuote.quotes.length > 0) {
          const bestRaw = Number(realQuote.quotes[0].quote.premium);
          const decimals = series?.settlement_decimals ?? 6;
          const bestScaled = bestRaw / Math.pow(10, decimals);
          return {
            strike,
            perUnit: amount > 0 ? bestScaled / amount : bestScaled,
            premiumDisplay: `${bestScaled.toFixed(2)} ${settlementSymbol}`,
            premium: bestScaled,
          };
        }
        return {
          strike,
          perUnit: 0,
          premiumDisplay: `— ${settlementSymbol}`,
          premium: 0,
        };
      }),
    [spot, amount, liveStrikes, settlementSymbol, quotesByBucket, series],
  );

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
    premiumDisplay: `0.00 ${settlementSymbol}`,
  };
  const selected: Strike = strikes[selectedIdx] ?? strikes[0] ?? placeholderStrike;
  const insufficientBtc = amount > btcBalance;
  const insufficientUsdc = (selected?.premium ?? 0) > usdcBalance;
  const insufficient = view === "writer" ? insufficientBtc : insufficientUsdc;

  // Map real RFQ quotes to the UI Quote type for the currently selected
  // bucket.
  const selectedBucketId = series?.buckets[selectedIdx]?.bucket_id ?? null;
  const realBucketQuote = selectedBucketId
    ? quotesByBucket[selectedBucketId]
    : undefined;

  useEffect(() => {
    if (realBucketQuote && realBucketQuote.quotes.length > 0) {
      const mapped: Quote[] = realBucketQuote.quotes.map((q, i) => {
        const premiumRaw = Number(q.quote.premium);
        const decimals = series?.settlement_decimals ?? 6;
        const premiumScaled = premiumRaw / Math.pow(10, decimals);
        return {
          id: `${q.mm_id}-${q.quote.nonce}`,
          name: shortId(q.mm_id),
          addr: shortId(q.quote.signer_account_id),
          fill: Math.round(q.mm_reputation * 100),
          revertRate: 0,
          latency: 0,
          premium: premiumScaled,
          ttl: Math.max(
            0,
            Math.round(
              (Number(q.quote.valid_until_ms) - Date.now()) / 1000,
            ),
          ),
          arrivedAt: realBucketQuote.updatedAt - i * 100,
        };
      });
      mapped.sort((a, b) =>
        view === "writer" ? b.premium - a.premium : a.premium - b.premium,
      );
      setQuotes(mapped);
    } else {
      setQuotes([]);
    }
  }, [realBucketQuote, rfqConnected, selectedIdx, amount, view, selected, series?.settlement_decimals]);

  const bestPremium = quotes[0]?.premium ?? (selected?.premium ?? 0);

  const submit = () => {
    if (insufficient || !connected || quotes.length === 0) return;
    setConfirmStage("signing");
    setTimeout(() => setConfirmStage("broadcast"), 1100);
    setTimeout(() => {
      const rangeStart = bucket.cursor + bucket.queued;
      const asset = series?.asset_symbol ?? "BTC";
      const expiry = series ? formatExpiry(series.expiry_iso) : "Jun 26th, 2026";
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
    }, 2400);
  };

  const closeConfirm = () => {
    const s = confirmSummary;
    setConfirmStage(null);
    if (s) {
      setToast(
        view === "writer"
          ? `position opened · +${s.premium.toFixed(2)} USDC received`
          : `call purchased · ${s.amount.toFixed(4)} BTC strike $${s.strike.toLocaleString("en-US")}`,
      );
      setTimeout(() => setToast(null), 4500);
    }
  };

  return {
    view,
    setView,
    connected,
    address: account?.address ?? null,
    spot,
    amount,
    setAmount,
    selectedIdx,
    setSelectedIdx,
    quotes,
    bestPremium,
    selected,
    strikes,
    insufficient,
    insufficientBtc,
    insufficientUsdc,
    btcBalance,
    usdcBalance,
    bucket,
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
