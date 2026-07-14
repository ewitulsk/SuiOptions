// Open orders for a bucket's DeepBook pool (SO-236). Lifted out of TradePanel so
// it can live as the "orders" tab in BuyDetailTabs. Self-contained: it owns the
// BalanceManager / open-order reads and the cancel / withdraw PTBs.

import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useCurrentAccount } from "@mysten/dapp-kit";

import type { Bucket, Series } from "../api/client";
import { posthog } from "../lib/posthog";
import {
  useBalanceManager,
  useBmBalances,
  useOpenOrders,
  useOpenOrderDetails,
  type PoolRef,
} from "../api/deepbook";
import { formatPrice } from "../format";
import { buildCancelAllTx, buildCancelOrderTx, buildWithdrawAllTx } from "../tx/deepbook";
import { useSubmitTransaction } from "../tx/submit";

type Props = {
  bucket: Bucket;
  series: Series;
};

const ctrlBtn = {
  fontSize: 11,
  cursor: "pointer",
  background: "transparent",
  border: "1px solid var(--aqua-line, rgba(92,107,122,0.25))",
  borderRadius: 6,
  padding: "2px 8px",
  color: "inherit",
} as const;

export function OpenOrdersSection({ bucket, series }: Props) {
  const account = useCurrentAccount();
  const submitTx = useSubmitTransaction();
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);

  const baseDec = series.asset_decimals ?? 8;
  const quoteDec = series.settlement_decimals ?? 6;
  const pool: PoolRef | null = bucket.deepbook_pool_id
    ? {
        poolId: bucket.deepbook_pool_id,
        baseCoinType: bucket.call_coin_type,
        quoteCoinType: series.settlement_coin_type,
        baseDecimals: baseDec,
        quoteDecimals: quoteDec,
      }
    : null;

  const addr = account?.address ?? null;
  const bm = useBalanceManager(addr);
  const openOrders = useOpenOrders(pool, bm.data ?? null, addr);
  const orderDetails = useOpenOrderDetails(pool, openOrders.data, addr);
  const bmBalances = useBmBalances(pool, bm.data ?? null, addr);

  const run = async (
    label: string,
    build: () => Parameters<typeof submitTx>[0],
    event?: { name: string; props?: Record<string, unknown> },
  ) => {
    setBusy(true);
    setNote(null);
    try {
      await submitTx(build());
      if (event) posthog.capture(event.name, { ...event.props, wallet_address: addr });
      setNote(`${label} submitted`);
      for (const key of [
        "deepbook-open-orders",
        "deepbook-open-order-details",
        "deepbook-bm-balances",
        "coin-balance",
      ]) {
        queryClient.invalidateQueries({ queryKey: [key] });
      }
    } catch (e) {
      posthog.captureException(e, { action: label, wallet_address: addr });
      setNote(`${label} failed: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setBusy(false);
    }
  };

  if (!pool || !bm.data) {
    return <div className="panel__sub">enable trading to see open orders</div>;
  }

  const bmId = bm.data;
  const bmBase = bmBalances.data?.baseRaw ?? 0n;
  const bmQuote = bmBalances.data?.quoteRaw ?? 0n;
  const settle = series.settlement_symbol;
  const poolRef = {
    poolId: pool.poolId,
    bmId,
    baseCoinType: pool.baseCoinType,
    quoteCoinType: pool.quoteCoinType,
  };
  const count = openOrders.data?.length ?? 0;

  return (
    <div style={{ fontSize: 13 }}>
      <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 6 }}>
        <span style={{ opacity: 0.8 }}>open orders ({count})</span>
        <span style={{ display: "flex", gap: 6 }}>
          {count > 0 && (
            <button
              disabled={busy}
              onClick={() =>
                void run("cancel all", () => buildCancelAllTx(poolRef), {
                  name: "deepbook_order_cancelled",
                  props: { scope: "all", pool_id: pool.poolId },
                })
              }
              style={ctrlBtn}
            >
              cancel all
            </button>
          )}
          {(bmBase > 0n || bmQuote > 0n) && addr && (
            <button
              disabled={busy}
              onClick={() =>
                void run(
                  "withdraw",
                  () => buildWithdrawAllTx({ ...poolRef, recipient: addr }),
                  { name: "deepbook_funds_withdrawn", props: { pool_id: pool.poolId } },
                )
              }
              style={ctrlBtn}
            >
              withdraw all
            </button>
          )}
        </span>
      </div>

      {count === 0 ? (
        <div className="panel__sub">no open orders</div>
      ) : (
        <>
          <div style={{ display: "flex", opacity: 0.6, fontSize: 10, textTransform: "uppercase", letterSpacing: 0.5, paddingBottom: 4 }}>
            <span style={{ flex: "0 0 44px" }}>side</span>
            <span style={{ flex: 1, textAlign: "right" }}>options</span>
            <span style={{ flex: 1, textAlign: "right" }}>price ({settle})</span>
            <span style={{ flex: "0 0 52px" }} />
          </div>
          {(openOrders.data ?? []).map((id) => {
            const d = orderDetails.data?.find((o) => o.orderId === id);
            return (
              <div key={id} style={{ display: "flex", alignItems: "center", marginTop: 4 }}>
                <span
                  style={{
                    flex: "0 0 44px",
                    fontWeight: 600,
                    color: d ? (d.isBid ? "var(--aqua-up, #1fbf75)" : "var(--aqua-down, #e15d6b)") : "inherit",
                  }}
                >
                  {d ? (d.isBid ? "buy" : "sell") : "—"}
                </span>
                <span style={{ flex: 1, textAlign: "right" }}>
                  {d ? d.remaining.toLocaleString(undefined, { maximumFractionDigits: 6 }) : "…"}
                </span>
                <span style={{ flex: 1, textAlign: "right" }}>{d ? formatPrice(d.price) : "…"}</span>
                <span style={{ flex: "0 0 52px", textAlign: "right" }}>
                  <button
                    disabled={busy}
                    onClick={() =>
                      void run("cancel", () => buildCancelOrderTx({ ...poolRef, orderId: BigInt(id) }), {
                        name: "deepbook_order_cancelled",
                        props: { scope: "single", order_id: id, pool_id: pool.poolId },
                      })
                    }
                    style={{ fontSize: 11, cursor: "pointer", background: "transparent", border: "none", color: "var(--aqua-down, #e15d6b)" }}
                  >
                    cancel
                  </button>
                </span>
              </div>
            );
          })}
        </>
      )}
      {note && <div className="panel__sub" style={{ marginTop: 6 }}>{note}</div>}
    </div>
  );
}
