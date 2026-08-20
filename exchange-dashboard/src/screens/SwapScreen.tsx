import { useMemo, useState } from "react";
import { useCurrentAccount } from "@mysten/dapp-kit";

import { OrderbookApiError } from "../api/orderbook";
import { explorerTxUrl } from "../config";
import { formatUnits, parseUnits, toBigint, typeName } from "../format";
import { useExchangeInfo } from "../hooks/useExchangeInfo";
import { marketsById, tokenUniverse, useMarkets } from "../hooks/useMarkets";
import { useCoinMetadataMap } from "../hooks/useCoinMetadata";
import { balanceKey, useWalletBalances } from "../hooks/useCoinBalance";
import { useDebounced, useRouteQuote } from "../hooks/useRouteQuote";
import { buildRouteFillTx } from "../tx/routeFill";
import { useSubmitTransaction } from "../tx/submit";

const SLIPPAGE_PRESETS = [
  { label: "0.1%", bps: 10n },
  { label: "0.5%", bps: 50n },
  { label: "1%", bps: 100n },
];

export function SwapScreen() {
  const account = useCurrentAccount();
  const submit = useSubmitTransaction();
  const exchangeQuery = useExchangeInfo();
  const packageId = exchangeQuery.data?.packageId;
  const marketsQuery = useMarkets();
  const markets = marketsQuery.data ?? [];
  const tokens = useMemo(() => tokenUniverse(markets), [markets]);
  const metaMap = useCoinMetadataMap(tokens).data ?? {};
  const balances = useWalletBalances().data;

  const [fromType, setFromType] = useState<string | null>(null);
  const [toType, setToType] = useState<string | null>(null);
  const [amountText, setAmountText] = useState("");
  const [slippageBps, setSlippageBps] = useState(50n);
  const [customSlippage, setCustomSlippage] = useState("");
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<{ ok: boolean; message: string; digest?: string } | null>(
    null,
  );

  const from = fromType ?? tokens[0] ?? null;
  const to = toType ?? tokens.find((t) => t !== from) ?? null;

  const label = (t: string) => metaMap[t]?.symbol || typeName(t);
  const decimalsOf = (t: string | null) => (t ? (metaMap[t]?.decimals ?? 0) : 0);
  const decIn = decimalsOf(from);
  const decOut = decimalsOf(to);

  let amountRaw: bigint | null = null;
  let amountError: string | null = null;
  if (amountText.trim()) {
    try {
      amountRaw = parseUnits(amountText, decIn);
      if (amountRaw === 0n) amountRaw = null;
    } catch (e) {
      amountError = e instanceof Error ? e.message : String(e);
    }
  }

  const debouncedRaw = useDebounced(amountRaw);
  const quoteQuery = useRouteQuote(from, to, debouncedRaw);
  const quote = quoteQuery.data;

  const expectedOut = quote ? toBigint(quote.plan.expectedOut) : null;
  const unrouted = quote ? toBigint(quote.plan.unrouted) : 0n;
  const routedIn = quote ? toBigint(quote.plan.input) : null;
  const minOut = expectedOut !== null ? (expectedOut * (10_000n - slippageBps)) / 10_000n : null;

  const balance = from && balances ? (balances.get(balanceKey(from)) ?? 0n) : null;
  const insufficient =
    balance !== null && debouncedRaw !== null && account ? balance < debouncedRaw : false;

  const impliedPrice =
    expectedOut !== null && routedIn !== null && routedIn > 0n && from && to
      ? Number(formatUnits(expectedOut, decOut)) / Number(formatUnits(routedIn, decIn))
      : null;

  const canSwap =
    !!account &&
    !!packageId &&
    !!quote &&
    expectedOut !== null &&
    expectedOut > 0n &&
    !insufficient &&
    !busy &&
    from !== to;

  function flip() {
    const f = from;
    setFromType(to);
    setToType(f);
    setAmountText("");
    setResult(null);
  }

  async function onSwap() {
    if (!quote || !from || !to || !account || !packageId || minOut === null) return;
    setBusy(true);
    setResult(null);
    try {
      const tx = buildRouteFillTx(quote, marketsById(markets), {
        sender: account.address,
        fromType: from,
        toType: to,
        minOut,
        packageId,
        whitelistId: exchangeQuery.data?.whitelistId ?? undefined,
      });
      const digest = await submit(tx);
      setResult({ ok: true, message: "Swap submitted", digest });
      setAmountText("");
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      // Ingress-gate aborts (whitelist::whitelist codes 1/2) are membership
      // problems, not stale quotes — say so instead of misattributing them.
      const gated =
        /assert_ingress_allowed/i.test(msg) || (/whitelist/i.test(msg) && /MoveAbort/i.test(msg));
      // Other aborts in settlement/order checks or the min-out guard almost
      // always mean the book moved out from under a stale quote.
      const stale = !gated && /abort|assert|ECoinBelowMin|MoveAbort/i.test(msg);
      setResult({
        ok: false,
        message: gated
          ? `Fill blocked by the ingress whitelist — this wallet is not whitelisted for the exchange domain (or ingress is paused).\n${msg}`
          : stale
            ? `Fill aborted on-chain — the quote likely went stale. Refresh and retry.\n${msg}`
            : msg,
      });
    } finally {
      setBusy(false);
    }
  }

  if (marketsQuery.isError) {
    const err = marketsQuery.error;
    return (
      <div className="card">
        <h1>Swap</h1>
        <div className="banner error">
          Can't reach the orderbook service:{" "}
          {err instanceof OrderbookApiError ? err.message : String(err)}
        </div>
      </div>
    );
  }

  return (
    <div className="card">
      <h1>Swap</h1>

      {!packageId && !exchangeQuery.isLoading && (
        <div className="banner warn">
          token-info reports no exchange deployment — quotes work, but swaps can't be executed.
        </div>
      )}

      <div className="field">
        <label>You pay</label>
        <div className="amount-row">
          <input
            inputMode="decimal"
            placeholder="0.0"
            value={amountText}
            onChange={(e) => {
              setAmountText(e.target.value);
              setResult(null);
            }}
          />
          <select value={from ?? ""} onChange={(e) => setFromType(e.target.value)}>
            {tokens.map((t) => (
              <option key={t} value={t}>
                {label(t)}
              </option>
            ))}
          </select>
        </div>
        {amountError && <div className="error">{amountError}</div>}
        {from && balance !== null && (
          <div className="hint">
            Balance: {formatUnits(balance, decIn)} {label(from)}
            {insufficient && <span style={{ color: "var(--red)" }}> — insufficient</span>}
          </div>
        )}
      </div>

      <div className="swap-arrow">
        <button className="copy-btn" onClick={flip} title="Flip direction">
          ↓↑
        </button>
      </div>

      <div className="field">
        <label>You receive</label>
        <div className="amount-row">
          <input
            readOnly
            value={expectedOut !== null ? formatUnits(expectedOut, decOut) : ""}
            placeholder="—"
          />
          <select value={to ?? ""} onChange={(e) => setToType(e.target.value)}>
            {tokens
              .filter((t) => t !== from)
              .map((t) => (
                <option key={t} value={t}>
                  {label(t)}
                </option>
              ))}
          </select>
        </div>
      </div>

      {quoteQuery.isError && (
        <div className="banner warn">
          {quoteQuery.error instanceof OrderbookApiError && quoteQuery.error.code === "NO_ROUTE"
            ? "No route — not enough liquidity between these tokens."
            : String(quoteQuery.error)}
        </div>
      )}

      {quote && expectedOut !== null && (
        <div className="quote-panel">
          {impliedPrice !== null && from && to && (
            <div className="quote-row">
              <span className="k">Price</span>
              <span className="v">
                1 {label(from)} ≈ {(impliedPrice).toPrecision(6)} {label(to)}
              </span>
            </div>
          )}
          <div className="quote-row">
            <span className="k">Route</span>
            <span className="v">
              {quote.plan.paths.length} path{quote.plan.paths.length === 1 ? "" : "s"},{" "}
              {quote.plan.paths.reduce((n, p) => n + p.hops.length, 0)} hop
              {quote.plan.paths.reduce((n, p) => n + p.hops.length, 0) === 1 ? "" : "s"}
            </span>
          </div>
          {minOut !== null && to && (
            <div className="quote-row">
              <span className="k">Min received</span>
              <span className="v">
                {formatUnits(minOut, decOut)} {label(to)}
              </span>
            </div>
          )}
          {unrouted > 0n && from && (
            <div className="quote-row">
              <span className="k">Unrouted</span>
              <span className="v warn">
                {formatUnits(unrouted, decIn)} {label(from)} (stays in wallet)
              </span>
            </div>
          )}
        </div>
      )}

      <div className="slippage">
        <span className="label">Slippage</span>
        {SLIPPAGE_PRESETS.map((p) => (
          <button
            key={p.label}
            className={slippageBps === p.bps && !customSlippage ? "active" : ""}
            onClick={() => {
              setSlippageBps(p.bps);
              setCustomSlippage("");
            }}
          >
            {p.label}
          </button>
        ))}
        <input
          placeholder="custom %"
          value={customSlippage}
          onChange={(e) => {
            const v = e.target.value;
            setCustomSlippage(v);
            const pct = Number(v);
            if (v && Number.isFinite(pct) && pct >= 0 && pct <= 50) {
              setSlippageBps(BigInt(Math.round(pct * 100)));
            }
          }}
        />
      </div>

      <button className="primary" disabled={!canSwap} onClick={onSwap}>
        {busy
          ? "Submitting…"
          : !account
            ? "Connect wallet"
            : quoteQuery.isFetching && !quote
              ? "Fetching quote…"
              : "Swap"}
      </button>

      {result && (
        <div className={`banner ${result.ok ? "success" : "error"}`} style={{ marginTop: 12 }}>
          {result.message}
          {result.digest && (
            <>
              {" — "}
              <a href={explorerTxUrl(result.digest)} target="_blank" rel="noreferrer">
                view on explorer
              </a>
            </>
          )}
        </div>
      )}
    </div>
  );
}
