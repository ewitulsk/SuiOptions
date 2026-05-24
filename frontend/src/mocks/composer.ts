// Composer state.
//
// Strikes are now live (sourced from api-service /buckets via useBuckets()).
// Everything else — wallet balances, spot price, MM quotes — is still mocked
// pending real wiring. The hook keeps a single return shape so UI components
// don't change as more pieces become live.
import { useEffect, useMemo, useState } from "react";
import { useCurrentAccount } from "@mysten/dapp-kit";
import { useBuckets } from "../api/useBuckets";
import type { Series } from "../api/client";
import type {
  Bucket,
  ConfirmStage,
  ConfirmSummary,
  Quote,
  Strike,
  View,
} from "../types";

// Used to compute mocked premiums by tile position. Real on-chain strikes
// drive the tile labels; these drive the premium math so the displayed
// premiums look the same as before this hook went live.
const MOCK_PREMIUM_STRIKES = [82000, 84000, 85000, 88000, 94000, 95000];

function mockPremiumPerUnit(spot: number, strike: number): number {
  const intrinsic = Math.max(0, spot - strike);
  const timeValue = Math.max(0, (1 - Math.abs(spot - strike) / spot) * spot * 0.025);
  return Math.max(intrinsic + timeValue, 0.001);
}

function formatExpiry(iso: string): string {
  if (!iso) return "—";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
}

type MmFixture = {
  name: string;
  addr: string;
  fill: number;
  revertRate: number;
  latency: number;
  driftW: number;
  driftT: number;
};

const MM_FIXTURE: MmFixture[] = [
  { name: "argonaut_mm_01", addr: "0x9f3a…42b1", fill: 98.4, revertRate: 0.003, latency: 142, driftW: 0,    driftT: 0    },
  { name: "phocas_capital", addr: "0x4ca1…07ee", fill: 96.1, revertRate: 0.008, latency: 188, driftW: -0.6, driftT: 0.7  },
  { name: "tidal_quants",   addr: "0xb2f8…11d0", fill: 99.2, revertRate: 0.001, latency: 96,  driftW: -1.4, driftT: 1.5  },
];

export type ComposerStateOpts = {
  initialView?: View;
  initialAmount?: number;
  initialIdx?: number;
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

  // Live strikes: take the first series the api-service returns, drop any
  // buckets the api-service couldn't scale (decimals unknown), and use the
  // resulting numeric strikes. Premiums stay mocked until the quoting
  // service is wired in — we compute them from a parallel mock strike grid
  // indexed by position so they don't blow up when on-chain test strikes
  // happen to be tiny (e.g. $0.13).
  const bucketsQuery = useBuckets();
  const series: Series | null = bucketsQuery.data?.[0] ?? null;
  const settlementSymbol = series?.settlement_symbol ?? "USDC";
  const liveStrikes: number[] = useMemo(
    () =>
      (series?.buckets ?? [])
        .map((b) => b.strike)
        .filter((s): s is number => s !== null),
    [series],
  );

  const strikes = useMemo<Strike[]>(
    () =>
      liveStrikes.map((strike, idx) => {
        // Premium math is mocked — anchor it to a synthetic strike at this
        // tile's position so values stay in the same range as before
        // regardless of the real on-chain strike.
        const mockStrike = MOCK_PREMIUM_STRIKES[idx % MOCK_PREMIUM_STRIKES.length];
        const perUnit = mockPremiumPerUnit(spot, mockStrike);
        const total = perUnit * amount;
        return {
          strike,
          perUnit,
          premiumDisplay: `${total.toFixed(2)} ${settlementSymbol}`,
          premium: total,
        };
      }),
    [spot, amount, liveStrikes, settlementSymbol],
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

  const bucketsLoading = bucketsQuery.isLoading;
  const bucketsEmpty = !bucketsLoading && strikes.length === 0;

  // Stream MM quotes whenever selection / amount / view changes
  useEffect(() => {
    if (!selected) return;
    setQuotes([]);
    const timers = MM_FIXTURE.map((n, i) =>
      setTimeout(() => {
        setQuotes((qs) => {
          const drift = view === "writer" ? n.driftW : n.driftT;
          const next: Quote[] = [
            ...qs,
            {
              id: `${selected.strike}-${n.addr}`,
              name: n.name,
              addr: n.addr,
              fill: n.fill,
              revertRate: n.revertRate,
              latency: n.latency,
              premium: +(selected.premium + drift).toFixed(2),
              ttl: 30 - i * 3,
              arrivedAt: Date.now(),
            },
          ];
          next.sort((a, b) =>
            view === "writer" ? b.premium - a.premium : a.premium - b.premium,
          );
          return next;
        });
      }, 400 + i * 600),
    );
    return () => timers.forEach(clearTimeout);
  }, [selectedIdx, amount, view, selected]);

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
  };
}
