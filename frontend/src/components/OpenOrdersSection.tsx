// Open exchange orders for a bucket's market (SO-236/SO-416). Lives as the
// "orders" tab in BuyDetailTabs. Self-contained: it owns the open-order read,
// the signed soft-cancel (a personal-message DELETE to the orderbook — no
// transaction), and the maker-escrow withdraw PTBs.
import { optionCoinType } from "../api/client";

import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useCurrentAccount, useSignPersonalMessage } from "@mysten/dapp-kit";
import { normalizeStructTag } from "@mysten/sui/utils";

import type { Bucket, Series } from "../api/client";
import { posthog } from "../lib/posthog";
import {
  cancelOrder,
  toBigint,
  useEscrowBalances,
  useExchangeMarketFor,
  useOpenExchangeOrders,
  type AccountOrder,
} from "../api/orderbook";
import { useExchangeBalanceManager } from "../api/exchangeAccount";
import { useSuiNetwork } from "../lib/suiGrpc";
import { formatPrice } from "../format";
import { buildCancelMessage, digestFromHex, splitWalletSignature } from "../tx/exchangeOrders";
import { buildEscrowWithdrawTx } from "../tx/exchangeEscrow";
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
  const network = useSuiNetwork();
  const { mutateAsync: signPersonalMessage } = useSignPersonalMessage();
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);

  const baseDec = series.asset_decimals ?? 8;
  const quoteDec = series.settlement_decimals ?? 6;
  const baseType = normalizeStructTag(optionCoinType(bucket));
  const quoteType = normalizeStructTag(series.settlement_coin_type);

  const { market } = useExchangeMarketFor(bucket);
  const addr = account?.address ?? null;
  const orders = useOpenExchangeOrders(addr);
  const bm = useExchangeBalanceManager(addr, network);
  const escrow = useEscrowBalances(bm.data ?? null);

  const run = async (
    label: string,
    action: () => Promise<unknown>,
    event?: { name: string; props?: Record<string, unknown> },
  ) => {
    setBusy(true);
    setNote(null);
    try {
      await action();
      if (event) posthog.capture(event.name, { ...event.props, wallet_address: addr });
      setNote(`${label} submitted`);
      for (const key of [
        "exchange-open-orders",
        "exchange-book",
        "exchange-escrow-balances",
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

  if (!market) {
    return <div className="panel__sub">no exchange market listed for this strike yet</div>;
  }
  if (!addr) {
    return <div className="panel__sub">connect a wallet to see open orders</div>;
  }

  const open = (orders.data ?? []).filter(
    (o) => o.order.registryId === market.registryId && o.status === "OPEN",
  );
  const settle = series.settlement_symbol;
  const count = open.length;

  /** Side / price / remaining base, derived from the signed order terms. */
  const rowView = (o: AccountOrder) => {
    const isBid = normalizeStructTag(o.order.makerToken) === quoteType;
    const baseRaw = toBigint(isBid ? o.order.takerAmount : o.order.makerAmount);
    const quoteRaw = toBigint(isBid ? o.order.makerAmount : o.order.takerAmount);
    const base = Number(baseRaw) / 10 ** baseDec;
    const price = base > 0 ? Number(quoteRaw) / 10 ** quoteDec / base : 0;
    // filledTaker accrues in TAKER-token units.
    const filledTaker = Number(toBigint(o.filledTaker));
    const takerTotal = Number(toBigint(o.order.takerAmount));
    const fillFrac = takerTotal > 0 ? filledTaker / takerTotal : 0;
    const remaining = base * Math.max(0, 1 - fillFrac);
    return { isBid, price, remaining };
  };

  const cancel = (o: AccountOrder) =>
    void run(
      "cancel",
      async () => {
        const message = buildCancelMessage(digestFromHex(o.digest));
        const { signature } = await signPersonalMessage({ message });
        const sig = splitWalletSignature(signature);
        await cancelOrder(o.digest, {
          scheme: sig.scheme,
          signature: sig.signature,
          publicKey: sig.publicKey,
        });
      },
      {
        name: "exchange_order_cancelled",
        props: { order_digest: o.digest, market_id: market.registryId },
      },
    );

  const escrowBase = escrow.data?.[baseType] ?? 0n;
  const escrowQuote = escrow.data?.[quoteType] ?? 0n;

  const withdraw = (coinType: string, amount: bigint, label: string) =>
    void run(
      `withdraw ${label}`,
      () =>
        submitTx(
          buildEscrowWithdrawTx({ bmId: bm.data!, coinType, amount, recipient: addr }),
        ),
      { name: "exchange_escrow_withdrawn", props: { coin_type: coinType } },
    );

  return (
    <div style={{ fontSize: 13 }}>
      <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 6 }}>
        <span style={{ opacity: 0.8 }}>open orders ({count})</span>
        {bm.data && (escrowBase > 0n || escrowQuote > 0n) && (
          <span style={{ display: "flex", gap: 6 }}>
            {escrowQuote > 0n && (
              <button
                disabled={busy}
                onClick={() => withdraw(quoteType, escrowQuote, settle)}
                style={ctrlBtn}
                title="Withdraw settlement escrow back to your wallet"
              >
                withdraw {formatPrice(Number(escrowQuote) / 10 ** quoteDec)} {settle}
              </button>
            )}
            {escrowBase > 0n && (
              <button
                disabled={busy}
                onClick={() => withdraw(baseType, escrowBase, "options")}
                style={ctrlBtn}
                title="Withdraw option-coin escrow back to your wallet"
              >
                withdraw {Number(escrowBase) / 10 ** baseDec} options
              </button>
            )}
          </span>
        )}
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
          {open.map((o) => {
            const d = rowView(o);
            return (
              <div key={o.digest} style={{ display: "flex", alignItems: "center", marginTop: 4 }}>
                <span
                  style={{
                    flex: "0 0 44px",
                    fontWeight: 600,
                    color: d.isBid ? "var(--aqua-up, #1fbf75)" : "var(--aqua-down, #e15d6b)",
                  }}
                >
                  {d.isBid ? "buy" : "sell"}
                </span>
                <span style={{ flex: 1, textAlign: "right" }}>
                  {d.remaining.toLocaleString(undefined, { maximumFractionDigits: 6 })}
                </span>
                <span style={{ flex: 1, textAlign: "right" }}>{formatPrice(d.price)}</span>
                <span style={{ flex: "0 0 52px", textAlign: "right" }}>
                  <button
                    disabled={busy}
                    onClick={() => cancel(o)}
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
