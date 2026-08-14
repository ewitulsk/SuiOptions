// DeepBook trade panel (SO-157): market + limit orders on a bucket's pool,
// side-by-side with the RFQ mint flow on the Buy screen.
//
// Funds model: each order's PTB deposits exactly what it needs from the
// wallet into the user's BalanceManager (created once via "Enable trading");
// proceeds and cancelled funds accumulate as BM balances and "Withdraw all"
// drains both assets back to the wallet. Taker fees are paid in the input
// token (pay_with_deep=false, 1.25× penalty — no DEEP needed, ever).
import { optionCoinType } from "../api/client";

import { useMemo, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useCurrentAccount } from "@mysten/dapp-kit";

import type { Bucket, Series } from "../api/client";
import { posthog } from "../lib/posthog";
import {
  useBalanceManager,
  useBmBalances,
  useOrderBook,
  type PoolRef,
} from "../api/deepbook";
import { useCoinBalance } from "../api/useCoinBalance";
import { useSegmentPill } from "../lib/useSegmentPill";
import { TokenLogo } from "./TokenLogo";
import { formatPrice } from "../format";
import {
  bidNotional,
  buildEnableTradingTx,
  buildPlaceLimitOrderTx,
  buildPlaceMarketOrderTx,
  deriveVenueParams,
  toRawPrice,
  toRawQty,
} from "../tx/deepbook";
import { useSubmitTransaction } from "../tx/submit";

type Props = {
  bucket: Bucket;
  series: Series;
};

type Tab = "market" | "limit";
type Side = "buy" | "sell";

export function TradePanel({ bucket, series }: Props) {
  const account = useCurrentAccount();
  const connected = !!account;
  const submitTx = useSubmitTransaction();
  const queryClient = useQueryClient();

  const [tab, setTab] = useState<Tab>("market");
  const [side, setSide] = useState<Side>("buy");
  const { ref: sideRef, geom: sidePill, animated: sideAnimated } = useSegmentPill(side);
  const [qtyStr, setQtyStr] = useState("0.01");
  const [priceStr, setPriceStr] = useState("");
  const [slippagePct, setSlippagePct] = useState(1);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);

  const baseDec = series.asset_decimals ?? 8;
  const quoteDec = series.settlement_decimals ?? 6;
  const pool: PoolRef | null = bucket.deepbook_pool_id
    ? {
        poolId: bucket.deepbook_pool_id,
        baseCoinType: optionCoinType(bucket),
        quoteCoinType: series.settlement_coin_type,
        baseDecimals: baseDec,
        quoteDecimals: quoteDec,
      }
    : null;
  const venue = deriveVenueParams(baseDec, quoteDec);

  const addr = account?.address ?? null;
  const bm = useBalanceManager(addr);
  const book = useOrderBook(pool, addr);
  const bmBalances = useBmBalances(pool, bm.data ?? null, addr);
  const walletQuote = useCoinBalance(addr, series.settlement_coin_type);
  const walletBase = useCoinBalance(addr, optionCoinType(bucket));
  const ownQuote = walletQuote.data ?? "0";
  const ownBase = walletBase.data ?? "0";

  const refreshAll = () => {
    for (const key of ["deepbook-book", "deepbook-open-orders", "deepbook-open-order-details", "deepbook-bm-balances", "coin-balance"]) {
      queryClient.invalidateQueries({ queryKey: [key] });
    }
  };

  const qty = Number(qtyStr) || 0;
  const qtyRaw = toRawQty(qty, baseDec, venue.lotSize);
  const limitPrice = Number(priceStr) || 0;

  // Market estimate: walk the book for `qty`.
  const marketEstimate = useMemo(() => {
    const levels = side === "buy" ? book.data?.asks : book.data?.bids;
    if (!levels || levels.length === 0 || qty <= 0) return null;
    let remaining = qty;
    let cost = 0;
    for (const l of levels) {
      const take = Math.min(remaining, l.qty);
      cost += take * l.price;
      remaining -= take;
      if (remaining <= 1e-12) break;
    }
    if (remaining > 1e-12) return { cost, partial: true };
    return { cost, partial: false };
  }, [book.data, side, qty]);

  const run = async (
    label: string,
    action: () => Promise<unknown>,
    event?: { name: string; props?: Record<string, unknown> },
  ) => {
    setBusy(true);
    setNote(null);
    try {
      await action();
      if (event) {
        posthog.capture(event.name, { ...event.props, wallet_address: addr });
      }
      setNote(`${label} submitted`);
      refreshAll();
    } catch (e) {
      posthog.captureException(e, {
        action: label,
        wallet_address: addr,
      });
      setNote(`${label} failed: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setBusy(false);
    }
  };

  if (!pool) return null;

  // ---- not yet enabled --------------------------------------------------
  if (!bm.isLoading && !bm.data) {
    return (
      <div className="panel">
        <div className="panel__head">
          trade on deepbook
        </div>
        <div className="panel__sub" style={{ marginBottom: 10 }}>
          One-time setup: create your DeepBook trading account (a shared
          BalanceManager object your orders settle through).
        </div>
        <button
          className="cta"
          style={{ width: "auto", padding: "10px 18px" }}
          disabled={!connected || busy}
          onClick={() => {
            run(
              "enable trading",
              () => submitTx(buildEnableTradingTx()),
              { name: "deepbook_trading_enabled" },
            ).then(() =>
              queryClient.invalidateQueries({ queryKey: ["deepbook-bm"] }),
            );
          }}
        >
          {!connected ? "Connect to trade" : busy ? "Setting up…" : "Enable trading"}
        </button>
        {note && <div className="panel__sub" style={{ marginTop: 8 }}>{note}</div>}
      </div>
    );
  }

  const bmId = bm.data!;
  const bmBase = bmBalances.data?.baseRaw ?? 0n;
  const bmQuote = bmBalances.data?.quoteRaw ?? 0n;
  const settle = series.settlement_symbol;

  const placeMarket = () => {
    if (qtyRaw <= 0n || !marketEstimate) return;
    const event = {
      name: "deepbook_order_placed",
      props: {
        order_type: "market",
        side,
        qty,
        estimated_cost: marketEstimate.cost,
        slippage_pct: slippagePct,
        pool_id: pool.poolId,
      },
    };
    const deposit =
      side === "buy"
        ? (() => {
            const cap = BigInt(
              Math.ceil(marketEstimate.cost * (1 + slippagePct / 100) * 10 ** quoteDec),
            );
            return { type: pool.quoteCoinType, amount: cap > bmQuote ? cap - bmQuote : 0n };
          })()
        : { type: pool.baseCoinType, amount: qtyRaw > bmBase ? qtyRaw - bmBase : 0n };
    void run(
      `market ${side}`,
      () =>
        submitTx(
          buildPlaceMarketOrderTx({
            poolId: pool.poolId,
            bmId,
            baseCoinType: pool.baseCoinType,
            quoteCoinType: pool.quoteCoinType,
            isBid: side === "buy",
            qtyRaw,
            depositCoinType: deposit.type,
            depositAmount: deposit.amount,
            recipient: addr ?? undefined,
          }),
        ),
      event,
    );
  };

  const placeLimit = () => {
    if (qtyRaw <= 0n || limitPrice <= 0) return;
    const priceRaw = toRawPrice(limitPrice, baseDec, quoteDec, venue.tickSize, side === "buy" ? "bid" : "ask");
    if (priceRaw <= 0n) return;
    const deposit =
      side === "buy"
        ? (() => {
            const need = bidNotional(priceRaw, qtyRaw);
            return { type: pool.quoteCoinType, amount: need > bmQuote ? need - bmQuote : 0n };
          })()
        : { type: pool.baseCoinType, amount: qtyRaw > bmBase ? qtyRaw - bmBase : 0n };
    void run(
      `limit ${side}`,
      () =>
        submitTx(
          buildPlaceLimitOrderTx({
            poolId: pool.poolId,
            bmId,
            baseCoinType: pool.baseCoinType,
            quoteCoinType: pool.quoteCoinType,
            isBid: side === "buy",
            qtyRaw,
            priceRaw,
            depositCoinType: deposit.type,
            depositAmount: deposit.amount,
          }),
        ),
      {
        name: "deepbook_order_placed",
        props: {
          order_type: "limit",
          side,
          qty,
          limit_price: limitPrice,
          notional: qty * limitPrice,
          pool_id: pool.poolId,
        },
      },
    );
  };

  const fmtQuote = (raw: bigint) => formatPrice(Number(raw) / 10 ** quoteDec);
  const fmtBase = (raw: bigint) => (Number(raw) / 10 ** baseDec).toString();

  return (
    <div className="panel">
      <div
        className="panel__head"
        style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}
      >
        <span>
          trade on deepbook
          {bucket.invalidated && (
            <span style={{ marginLeft: 8, fontSize: 10, opacity: 0.8 }}>
              · minting frozen — secondary trading open
            </span>
          )}
        </span>
        <span style={{ display: "flex", gap: 4 }}>
          {(["market", "limit"] as Tab[]).map((t) => (
            <button
              key={t}
              onClick={() => setTab(t)}
              style={{
                border: "none",
                borderRadius: 6,
                padding: "2px 10px",
                fontSize: 11,
                cursor: "pointer",
                background: tab === t ? "var(--aqua-line, rgba(92,107,122,0.18))" : "transparent",
                color: "inherit",
              }}
            >
              {t}
            </button>
          ))}
        </span>
      </div>

      {/* Trade ticket sits horizontally under the chart (SO-225): order form,
          holdings, and open orders side by side instead of a narrow rail. */}
      <div className="tradepanel__body">
      <div className="tradepanel__form">
        <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
          <div ref={sideRef} style={{ position: "relative", display: "flex", gap: 6 }}>
            <span
              className="tradepanel__side-pill"
              aria-hidden
              style={{
                position: "absolute",
                top: 0,
                left: 0,
                height: "100%",
                width: sidePill.width,
                transform: `translateX(${sidePill.left}px)`,
                opacity: sidePill.ready ? 1 : 0,
                transition: sideAnimated ? undefined : "none",
              }}
            />
            {(["buy", "sell"] as Side[]).map((s) => (
              <button
                key={s}
                className={side === s ? "is-active" : undefined}
                onClick={() => setSide(s)}
                style={{
                  position: "relative",
                  zIndex: 1,
                  flex: 1,
                  padding: "6px 0",
                  borderRadius: 8,
                  border: side === s
                    ? "1px solid transparent"
                    : "1px solid var(--aqua-line, rgba(92,107,122,0.25))",
                  cursor: "pointer",
                  background: "transparent",
                  color: side === s ? "#fff" : "inherit",
                  fontWeight: 600,
                  transition: "color 160ms ease",
                }}
              >
                {s}
              </button>
            ))}
          </div>

          <label style={{ fontSize: 11, opacity: 0.8 }}>
            options
            <input
              value={qtyStr}
              onChange={(e) => setQtyStr(e.target.value)}
              inputMode="decimal"
              style={{ width: "100%", padding: 6, borderRadius: 6, border: "1px solid var(--aqua-line, rgba(92,107,122,0.25))", background: "transparent", color: "inherit" }}
            />
          </label>

          {tab === "limit" ? (
            <label style={{ fontSize: 11, opacity: 0.8 }}>
              limit price ({settle})
              <input
                value={priceStr}
                onChange={(e) => setPriceStr(e.target.value)}
                inputMode="decimal"
                placeholder={book.data?.asks[0] ? formatPrice(book.data.asks[0].price) : ""}
                style={{ width: "100%", padding: 6, borderRadius: 6, border: "1px solid var(--aqua-line, rgba(92,107,122,0.25))", background: "transparent", color: "inherit" }}
              />
            </label>
          ) : (
            <div style={{ fontSize: 11, opacity: 0.8 }}>
              max slippage{" "}
              <select
                value={slippagePct}
                onChange={(e) => setSlippagePct(Number(e.target.value))}
                style={{ background: "transparent", color: "inherit", border: "1px solid var(--aqua-line, rgba(92,107,122,0.25))", borderRadius: 6, padding: 2 }}
              >
                {[0.5, 1, 2, 5].map((p) => (
                  <option key={p} value={p}>{p}%</option>
                ))}
              </select>
              {marketEstimate && (
                <div style={{ marginTop: 4 }}>
                  est. {side === "buy" ? "cost" : "proceeds"}: {formatPrice(marketEstimate.cost)} {settle}
                  {marketEstimate.partial && " · book too thin for full size"}
                </div>
              )}
            </div>
          )}

          <button
            className="cta"
            style={{ width: "100%", padding: "10px 0" }}
            disabled={
              busy ||
              !connected ||
              qtyRaw <= 0n ||
              (tab === "market" && (!marketEstimate || marketEstimate.partial)) ||
              (tab === "limit" && limitPrice <= 0)
            }
            onClick={tab === "market" ? placeMarket : placeLimit}
          >
            {busy ? "Submitting…" : `${tab} ${side} ${qty || ""} option${qty === 1 ? "" : "s"}`}
          </button>
          {note && <div className="panel__sub">{note}</div>}
          <div className="panel__sub" style={{ fontSize: 10 }}>
            wallet: {fmtQuote(BigInt(ownQuote))} {settle} ·{" "}
            {fmtBase(BigInt(ownBase))} options
          </div>
        </div>
      </div>

      {/* trading-account holdings — what the user has parked in DeepBook */}
      <div
        className="tradepanel__holdings"
        style={{
          padding: "8px 10px",
          borderRadius: 8,
          border: "1px solid var(--aqua-line, rgba(92,107,122,0.25))",
          background: "rgba(92,107,122,0.06)",
        }}
      >
        <div style={{ fontSize: 10, textTransform: "uppercase", letterSpacing: 0.5, opacity: 0.6 }}>
          in DeepBook trading account
        </div>
        <div style={{ display: "flex", gap: 16, marginTop: 4, alignItems: "center" }}>
          <span style={{ display: "inline-flex", alignItems: "center", gap: 6, fontSize: 14 }}>
            <TokenLogo
              symbol={settle}
              className="asset-glyph asset-glyph--sm"
              fallback={<span className="asset-glyph">{settle[0]}</span>}
            />
            <b>{fmtQuote(bmQuote)}</b> {settle}
          </span>
          <span style={{ fontSize: 14, opacity: 0.85 }}>
            <b>{fmtBase(bmBase)}</b> options
          </span>
        </div>
      </div>

      </div>
    </div>
  );
}
