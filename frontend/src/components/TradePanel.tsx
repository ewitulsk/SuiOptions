// Exchange trade panel (SO-416): market + limit orders on a bucket's
// in-house exchange market, side-by-side with the RFQ mint flow on the Buy
// screen.
//
// Funds model: MARKET orders are taker fills — a route quote from the
// orderbook is settled atomically from/into the wallet (no custody, ever).
// LIMIT orders are signed maker orders resting off-chain: the maker escrows
// `makerAmount` in their shared exchange BalanceManager (created once via
// "Enable escrow"), signs the order digest as a personal message, and POSTs
// it to the orderbook. If the bucket has no listed market yet, this panel
// offers the permissionless `exchange_listing` create-market PTB (SO-415).
import { optionCoinType, seriesOptionType } from "../api/client";

import { useMemo, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useCurrentAccount, useSignPersonalMessage } from "@mysten/dapp-kit";
import { normalizeStructTag, normalizeSuiAddress } from "@mysten/sui/utils";

import type { Bucket, Series } from "../api/client";
import { posthog } from "../lib/posthog";
import {
  getEscrowBalances,
  getMarketsInfo,
  getRoutes,
  postOrder,
  snapToLot,
  ticksFromPrice,
  toBigint,
  toDisplayBook,
  useEscrowBalances,
  useExchangeBook,
  useExchangeMarketFor,
  useOpenExchangeOrders,
  type SignedOrderWire,
} from "../api/orderbook";
import {
  cacheExchangeBalanceManager,
  useExchangeBalanceManager,
} from "../api/exchangeAccount";
import { useCoinBalance } from "../api/useCoinBalance";
import { useSuiGrpcClient, useSuiNetwork } from "../lib/suiGrpc";
import { useSegmentPill } from "../lib/useSegmentPill";
import { TokenLogo } from "./TokenLogo";
import { formatPrice } from "../format";
import { WHITELIST_ID } from "../config";
import {
  orderDigest,
  splitWalletSignature,
  ZERO_ADDRESS,
  type OrderFields,
} from "../tx/exchangeOrders";
import { buildRouteFillTx } from "../tx/exchangeFill";
import {
  buildEnableEscrowTx,
  buildEscrowDepositTx,
  resolveCreatedBalanceManager,
} from "../tx/exchangeEscrow";
import { buildListMarketTx, canListMarkets } from "../tx/exchangeListing";
import { useSubmitTransaction } from "../tx/submit";

type Props = {
  bucket: Bucket;
  series: Series;
};

type Tab = "market" | "limit";
type Side = "buy" | "sell";

/** Resting maker orders self-expire after this long. */
const ORDER_LIFETIME_MS = 7 * 24 * 3_600_000;

export function TradePanel({ bucket, series }: Props) {
  const account = useCurrentAccount();
  const connected = !!account;
  const submitTx = useSubmitTransaction();
  const queryClient = useQueryClient();
  const client = useSuiGrpcClient();
  const network = useSuiNetwork();
  const { mutateAsync: signPersonalMessage } = useSignPersonalMessage();

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
  const baseType = normalizeStructTag(optionCoinType(bucket));
  const quoteType = normalizeStructTag(series.settlement_coin_type);

  const { market, info } = useExchangeMarketFor(bucket);
  const marketId = market?.registryId ?? null;

  const addr = account?.address ?? null;
  const bm = useExchangeBalanceManager(addr, network);
  const bookQ = useExchangeBook(marketId);
  const book = toDisplayBook(bookQ.data, market, baseDec, quoteDec);
  const escrow = useEscrowBalances(bm.data ?? null);
  // The maker's OPEN orders (all markets) — escrow already committed to them
  // must stay covered when sizing the next deposit (intake §5.4 step 4).
  const myOrders = useOpenExchangeOrders(addr);
  const walletQuote = useCoinBalance(addr, series.settlement_coin_type);
  const walletBase = useCoinBalance(addr, optionCoinType(bucket));
  const ownQuote = walletQuote.data ?? "0";
  const ownBase = walletBase.data ?? "0";

  const refreshAll = () => {
    for (const key of [
      "exchange-book",
      "exchange-open-orders",
      "exchange-escrow-balances",
      "exchange-account-fills",
      "coin-balance",
    ]) {
      queryClient.invalidateQueries({ queryKey: [key] });
    }
  };

  const qty = Number(qtyStr) || 0;
  const qtyRawUnsnapped = BigInt(Math.max(0, Math.floor(qty * 10 ** baseDec)));
  const qtyRaw = market ? snapToLot(qtyRawUnsnapped, market) : qtyRawUnsnapped;
  const limitPrice = Number(priceStr) || 0;

  // Market estimate: walk the book for `qty`.
  const marketEstimate = useMemo(() => {
    const levels = side === "buy" ? book?.asks : book?.bids;
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
  }, [book, side, qty]);

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

  // ---- no market listed yet ---------------------------------------------
  if (!market) {
    if (bucket.bucket_id === null) return null;
    // Backend already knows a registry the orderbook hasn't discovered yet:
    // listing again would abort (EAlreadyListed) — just wait it out.
    if (bucket.exchange_market_id) {
      return (
        <div className="panel">
          <div className="panel__head">trade on exchange</div>
          <div className="panel__sub">
            market listed — waiting for the orderbook to pick it up…
          </div>
        </div>
      );
    }
    const listable = canListMarkets();
    const isPut = seriesOptionType(series) === "put";
    return (
      <div className="panel">
        <div className="panel__head">trade on exchange</div>
        <div className="panel__sub" style={{ marginBottom: 10 }}>
          {listable
            ? "This strike has no exchange market yet. Anyone can list one — a one-time, permissionless transaction."
            : "This strike has no exchange market yet, and market listing isn't available on this deployment."}
        </div>
        {listable && (
          <button
            className="cta"
            style={{ width: "auto", padding: "10px 18px" }}
            disabled={!connected || busy}
            onClick={() =>
              void run(
                "list market",
                async () => {
                  await submitTx(
                    buildListMarketTx({
                      bucketId: bucket.bucket_id!,
                      optionCoinType: optionCoinType(bucket),
                      isPut,
                    }),
                  );
                  // Discovery lands within seconds — poll the orderbook until
                  // the new registry shows up, then refresh the market list.
                  const base = normalizeStructTag(optionCoinType(bucket));
                  for (let i = 0; i < 15; i++) {
                    await new Promise((r) => setTimeout(r, 2_000));
                    try {
                      const fresh = await getMarketsInfo();
                      if (fresh.markets.some((m) => normalizeStructTag(m.base) === base)) break;
                    } catch {
                      // orderbook briefly unreachable — keep polling
                    }
                  }
                  queryClient.invalidateQueries({ queryKey: ["exchange-markets"] });
                  queryClient.invalidateQueries({ queryKey: ["buckets"] });
                },
                { name: "exchange_market_listed", props: { bucket_id: bucket.bucket_id } },
              )
            }
          >
            {!connected ? "Connect to list" : busy ? "Listing…" : "List market"}
          </button>
        )}
        {note && <div className="panel__sub" style={{ marginTop: 8 }}>{note}</div>}
      </div>
    );
  }

  const settle = series.settlement_symbol;
  const escrowBase = escrow.data?.[baseType] ?? 0n;
  const escrowQuote = escrow.data?.[quoteType] ?? 0n;

  // ---- taker market order (route fill) ----------------------------------
  const placeMarket = () => {
    if (qtyRaw <= 0n || !marketEstimate || !addr || !info) return;
    const event = {
      name: "exchange_order_placed",
      props: {
        order_type: "market",
        side,
        qty,
        estimated_cost: marketEstimate.cost,
        slippage_pct: slippagePct,
        market_id: marketId,
      },
    };
    const slip = slippagePct / 100;
    const fromType = side === "buy" ? quoteType : baseType;
    const toType = side === "buy" ? baseType : quoteType;
    const amountIn =
      side === "buy"
        ? BigInt(Math.ceil(marketEstimate.cost * 10 ** quoteDec))
        : qtyRaw;
    const minOut =
      side === "buy"
        ? BigInt(Math.floor(Number(qtyRaw) * (1 - slip)))
        : BigInt(Math.floor(marketEstimate.cost * (1 - slip) * 10 ** quoteDec));
    void run(
      `market ${side}`,
      async () => {
        const quote = await getRoutes(fromType, toType, amountIn);
        const marketsById = new Map(info.markets.map((m) => [m.registryId, m]));
        const tx = buildRouteFillTx(quote, marketsById, {
          sender: addr,
          fromType,
          toType,
          minOut,
          packageId: info.packageId,
          whitelistId: info.whitelistId ?? WHITELIST_ID,
        });
        await submitTx(tx);
      },
      event,
    );
  };

  // ---- maker limit order (signed, escrow-backed) ------------------------
  const enableEscrow = () => {
    void run(
      "enable escrow",
      async () => {
        const digest = await submitTx(buildEnableEscrowTx());
        const bmId = await resolveCreatedBalanceManager(client, digest);
        if (addr) cacheExchangeBalanceManager(addr, bmId);
        queryClient.invalidateQueries({ queryKey: ["exchange-bm"] });
      },
      { name: "exchange_escrow_enabled" },
    );
  };

  const placeLimit = () => {
    if (qtyRaw <= 0n || limitPrice <= 0 || !addr || !bm.data || !market) return;
    const ticks = ticksFromPrice(limitPrice, market, baseDec, quoteDec, side === "buy" ? "bid" : "ask");
    const lot = toBigint(market.lotSize);
    if (ticks <= 0n || lot <= 0n) return;
    // Exact-tick notional: quoteRaw = ticks × tickSize × (baseRaw / lotSize)
    // (baseRaw is lot-snapped, so the book's price grid divides exactly).
    const quoteRaw = ticks * toBigint(market.tickSize) * (qtyRaw / lot);
    if (quoteRaw <= 0n) return;

    const isBid = side === "buy";
    const order: OrderFields = {
      makerToken: isBid ? quoteType : baseType,
      takerToken: isBid ? baseType : quoteType,
      makerAmount: isBid ? quoteRaw : qtyRaw,
      takerAmount: isBid ? qtyRaw : quoteRaw,
      maxFeeBps: toBigint(market.currentFeeBps),
      maker: addr,
      makerManagerId: bm.data,
      taker: ZERO_ADDRESS,
      sender: ZERO_ADDRESS,
      expiryMs: Date.now() + ORDER_LIFETIME_MS,
      salt: BigInt(Date.now()) * 1_000n + BigInt(Math.floor(Math.random() * 1_000)),
    };
    // Escrow the orderbook already holds against OPEN orders in this token
    // stays committed (intake §5.4 step 4) — the new order needs headroom on
    // top of it. Ignoring partial fills over-deposits slightly; safe side.
    const bmId = bm.data;
    const committed = (myOrders.data ?? [])
      .filter(
        (o) =>
          o.status === "OPEN" &&
          normalizeSuiAddress(o.order.makerManagerId) === normalizeSuiAddress(bmId) &&
          normalizeStructTag(o.order.makerToken) === order.makerToken,
      )
      .reduce((sum, o) => sum + toBigint(o.order.makerAmount), 0n);
    const escrowHeld = isBid ? escrowQuote : escrowBase;
    const needed = committed + order.makerAmount;
    const shortfall = needed > escrowHeld ? needed - escrowHeld : 0n;

    void run(
      `limit ${side}`,
      async () => {
        // Top up the escrow so the resting order is fully backed, then wait
        // for the orderbook's chain mirror to see the deposit — intake
        // rejects orders whose escrow it can't yet observe.
        if (shortfall > 0n) {
          await submitTx(
            buildEscrowDepositTx({
              bmId,
              coinType: order.makerToken,
              amount: shortfall,
            }),
          );
          let mirrored = false;
          for (let i = 0; i < 15; i++) {
            await new Promise((r) => setTimeout(r, 2_000));
            try {
              const balances = await getEscrowBalances(bmId);
              const held = balances.find(
                (b) => normalizeStructTag(b.token) === order.makerToken,
              );
              if (held && toBigint(held.amount) >= needed) {
                mirrored = true;
                break;
              }
            } catch {
              // orderbook briefly unreachable — keep polling
            }
          }
          if (!mirrored) {
            throw new Error(
              "escrow deposit not yet visible to the orderbook — funds are safe in your escrow; retry the order in a moment",
            );
          }
        }
        const digest = orderDigest(order, market.registryId);
        const { signature } = await signPersonalMessage({ message: digest });
        const sig = splitWalletSignature(signature);
        const wire: SignedOrderWire = {
          makerToken: order.makerToken,
          takerToken: order.takerToken,
          makerAmount: order.makerAmount.toString(),
          takerAmount: order.takerAmount.toString(),
          maxFeeBps: order.maxFeeBps.toString(),
          maker: order.maker,
          makerManagerId: order.makerManagerId,
          taker: order.taker,
          sender: order.sender,
          expiryMs: order.expiryMs,
          salt: order.salt.toString(),
          registryId: market.registryId,
          scheme: sig.scheme,
          signature: sig.signature,
          publicKey: sig.publicKey,
        };
        const res = await postOrder(wire);
        if (res.status === "SELF_TRADE_CANCELLED") {
          throw new Error("order crossed your own resting order and was cancelled");
        }
      },
      {
        name: "exchange_order_placed",
        props: {
          order_type: "limit",
          side,
          qty,
          limit_price: limitPrice,
          notional: qty * limitPrice,
          market_id: marketId,
        },
      },
    );
  };

  const fmtQuote = (raw: bigint) => formatPrice(Number(raw) / 10 ** quoteDec);
  const fmtBase = (raw: bigint) => (Number(raw) / 10 ** baseDec).toString();

  const needsEscrow = tab === "limit" && !bm.isLoading && !bm.data;

  return (
    <div className="panel">
      <div
        className="panel__head"
        style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}
      >
        <span>
          trade on exchange
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

      {/* Trade ticket sits horizontally under the chart (SO-225): order form
          and holdings side by side instead of a narrow rail. */}
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
                placeholder={book?.asks[0] ? formatPrice(book.asks[0].price) : ""}
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

          {needsEscrow ? (
            <>
              <div className="panel__sub">
                One-time setup for resting orders: create your exchange escrow
                account (a shared BalanceManager your maker orders settle
                through). Market orders never need it.
              </div>
              <button
                className="cta"
                style={{ width: "100%", padding: "10px 0" }}
                disabled={!connected || busy}
                onClick={enableEscrow}
              >
                {!connected ? "Connect to trade" : busy ? "Setting up…" : "Enable escrow"}
              </button>
            </>
          ) : (
            <button
              className="cta"
              style={{ width: "100%", padding: "10px 0" }}
              disabled={
                busy ||
                !connected ||
                qtyRaw <= 0n ||
                (tab === "market" && (!marketEstimate || marketEstimate.partial)) ||
                (tab === "limit" && (limitPrice <= 0 || bm.isLoading))
              }
              onClick={tab === "market" ? placeMarket : placeLimit}
            >
              {busy ? "Submitting…" : `${tab} ${side} ${qty || ""} option${qty === 1 ? "" : "s"}`}
            </button>
          )}
          {note && <div className="panel__sub">{note}</div>}
          <div className="panel__sub" style={{ fontSize: 10 }}>
            wallet: {fmtQuote(BigInt(ownQuote))} {settle} ·{" "}
            {fmtBase(BigInt(ownBase))} options
          </div>
        </div>
      </div>

      {/* maker escrow — what the user has parked in the exchange */}
      {bm.data && (
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
            in exchange escrow
          </div>
          <div style={{ display: "flex", gap: 16, marginTop: 4, alignItems: "center" }}>
            <span style={{ display: "inline-flex", alignItems: "center", gap: 6, fontSize: 14 }}>
              <TokenLogo
                symbol={settle}
                className="asset-glyph asset-glyph--sm"
                fallback={<span className="asset-glyph">{settle[0]}</span>}
              />
              <b>{fmtQuote(escrowQuote)}</b> {settle}
            </span>
            <span style={{ fontSize: 14, opacity: 0.85 }}>
              <b>{fmtBase(escrowBase)}</b> options
            </span>
          </div>
        </div>
      )}

      </div>
    </div>
  );
}
