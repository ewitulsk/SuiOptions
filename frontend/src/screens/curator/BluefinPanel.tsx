// Bluefin trading tab (SO-305): the curator's authorized-wallet trading
// surface — account summary, order ticket, open orders, recent fills.
// Everything here uses the curator's OWN wallet (signPersonalMessage) — no
// FROST ceremony on the trading path (that's the whole point of the
// authorized wallet). Data + order relay go through the hedge-signer proxy.

import { useCallback, useEffect, useState } from "react";
import { useCurrentAccount, useSignPersonalMessage } from "@mysten/dapp-kit";

import {
  authorizedWalletAddresses,
  bluefinLogin,
  cancelOrders,
  fetchAccountTrades,
  fetchBluefinAccount,
  fetchBluefinExchangeInfo,
  fetchOpenOrders,
  fromE9,
  loginPayload,
  orderPayload,
  placeOrder,
  toE9,
  type BluefinAccount,
  type BluefinExchangeInfo,
  type BluefinMarket,
  type BluefinOpenOrder,
  type BluefinTrade,
  type OrderTicket,
} from "../../api/bluefin";
import { curatorFieldStyle } from "./styles";

const fmt = (n: number | null, digits = 2) =>
  n == null ? "—" : n.toLocaleString(undefined, { maximumFractionDigits: digits });

export function BluefinPanel({ parentAddress }: { parentAddress: string }) {
  const account = useCurrentAccount();
  const { mutateAsync: signPersonalMessage } = useSignPersonalMessage();

  const [info, setInfo] = useState<BluefinExchangeInfo | null>(null);
  const [jwt, setJwt] = useState<string | null>(null);
  const [acct, setAcct] = useState<BluefinAccount | null>(null);
  const [orders, setOrders] = useState<BluefinOpenOrder[]>([]);
  const [trades, setTrades] = useState<BluefinTrade[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  useEffect(() => {
    fetchBluefinExchangeInfo().then(setInfo).catch((e) => setError(String(e)));
  }, []);

  // Log in with the curator wallet → JWT scoped to trade for the parent.
  const login = useCallback(async () => {
    if (!account) return null;
    const { json, bytes } = loginPayload(account.address);
    const { signature } = await signPersonalMessage({ message: bytes });
    const tokens = await bluefinLogin(json, signature);
    setJwt(tokens.accessToken);
    return tokens.accessToken;
  }, [account, signPersonalMessage]);

  const refresh = useCallback(
    async (token: string) => {
      const [a, o, t] = await Promise.all([
        fetchBluefinAccount(parentAddress),
        fetchOpenOrders(token),
        fetchAccountTrades(token),
      ]);
      setAcct(a);
      setOrders(o);
      setTrades(t);
    },
    [parentAddress],
  );

  const onConnect = async () => {
    setBusy("connecting");
    setError(null);
    try {
      const token = await login();
      if (token) await refresh(token);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  };

  const authorized =
    acct != null && account != null
      ? authorizedWalletAddresses(acct).some(
          (w) => w.toLowerCase() === account.address.toLowerCase(),
        )
      : null;

  if (!account) {
    return (
      <div className="vault-card__body vault-prose__muted">
        Connect the authorized trading wallet to trade on Bluefin.
      </div>
    );
  }

  if (!jwt) {
    return (
      <div className="vault-card__body">
        <div className="vault-prose__muted" style={{ fontSize: 12, marginBottom: 8 }}>
          Sign in to Bluefin with your authorized wallet. This is a wallet
          signature only — no ceremony, no gas.
        </div>
        <button className="vault-invest__cta" disabled={busy != null} onClick={onConnect}>
          {busy === "connecting" ? "Signing in…" : "Sign in to Bluefin"}
        </button>
        {error && (
          <div className="status-pill is-danger" style={{ display: "block", marginTop: 8, fontSize: 12 }}>
            ⚠ {error}
          </div>
        )}
      </div>
    );
  }

  return (
    <div className="vault-card__body">
      {authorized === false && (
        <div className="status-pill is-danger" style={{ display: "block", fontSize: 12, marginBottom: 10 }}>
          ⚠ This wallet is not an authorized trader on the parent account.
          Complete the setup wizard's authorize step first.
        </div>
      )}
      <AccountSummary acct={acct} />
      <OrderTicketForm
        markets={info?.markets ?? []}
        idsId={info?.contractsConfig.idsId ?? ""}
        parentAddress={parentAddress}
        disabled={busy != null}
        onSubmit={async (ticket) => {
          setBusy("placing");
          setError(null);
          try {
            const payload = orderPayload(info!.contractsConfig.idsId, ticket);
            const { signature } = await signPersonalMessage({ message: payload.bytes });
            await placeOrder(jwt, ticket, info!.contractsConfig.idsId, payload.salt, payload.signedAtMillis, signature);
            await refresh(jwt);
          } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
          } finally {
            setBusy(null);
          }
        }}
      />
      {error && (
        <div className="status-pill is-danger" style={{ display: "block", margin: "8px 0", fontSize: 12 }}>
          ⚠ {error}
        </div>
      )}
      <OpenOrders
        orders={orders}
        busy={busy != null}
        onCancel={async (symbol, hash) => {
          setBusy("cancelling");
          try {
            await cancelOrders(jwt, symbol, [hash]);
            await refresh(jwt);
          } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
          } finally {
            setBusy(null);
          }
        }}
      />
      <RecentFills trades={trades} />
      <button className="vault-invest__tab" style={{ marginTop: 8 }} onClick={() => void refresh(jwt)}>
        Refresh
      </button>
    </div>
  );
}

function AccountSummary({ acct }: { acct: BluefinAccount | null }) {
  if (!acct) {
    return (
      <div className="vault-card__body vault-prose__muted" style={{ padding: 0, marginBottom: 10 }}>
        No Bluefin account data yet.
      </div>
    );
  }
  return (
    <div className="vault-kv" style={{ marginBottom: 12 }}>
      <div className="vault-kv__row">
        <span>Equity</span>
        <span>{fmt(fromE9(acct.totalAccountValueE9))} USDC</span>
      </div>
      <div className="vault-kv__row">
        <span>Margin available</span>
        <span>{fmt(fromE9(acct.marginAvailableE9))} USDC</span>
      </div>
      <div className="vault-kv__row">
        <span>Unrealized PnL</span>
        <span>{fmt(fromE9(acct.totalUnrealizedPnlE9))} USDC</span>
      </div>
      <div className="vault-kv__row">
        <span>Leverage</span>
        <span>{fmt(fromE9(acct.crossLeverageE9))}×</span>
      </div>
      {(acct.positions ?? []).length > 0 && (
        <>
          <div className="vault-kv__row" style={{ fontWeight: 600, marginTop: 6 }}>
            <span>Position</span>
            <span>Size · Entry · uPnL</span>
          </div>
          {acct.positions!.map((p) => (
            <div className="vault-kv__row" key={p.symbol + p.side}>
              <span>
                {p.symbol} {p.side}
              </span>
              <span>
                {fmt(fromE9(p.sizeE9), 4)} · {fmt(fromE9(p.avgEntryPriceE9))} ·{" "}
                {fmt(fromE9(p.unrealizedPnlE9))}
              </span>
            </div>
          ))}
        </>
      )}
    </div>
  );
}

function OrderTicketForm({
  markets,
  idsId,
  parentAddress,
  disabled,
  onSubmit,
}: {
  markets: BluefinMarket[];
  idsId: string;
  parentAddress: string;
  disabled: boolean;
  onSubmit: (t: OrderTicket) => void;
}) {
  const tradable = markets.filter((m) => m.status === "ACTIVE" || m.status === "TRADING" || true);
  const [symbol, setSymbol] = useState(tradable[0]?.symbol ?? "");
  const [side, setSide] = useState<"LONG" | "SHORT">("LONG");
  const [type, setType] = useState<"LIMIT" | "MARKET">("LIMIT");
  const [price, setPrice] = useState("");
  const [size, setSize] = useState("");
  const [leverage, setLeverage] = useState("1");
  const [reduceOnly, setReduceOnly] = useState(false);

  const market = tradable.find((m) => m.symbol === symbol) ?? tradable[0];
  const sizeNum = Number(size) || 0;
  const priceNum = Number(price) || 0;
  const valid =
    idsId !== "" &&
    market != null &&
    sizeNum > 0 &&
    (type === "MARKET" || priceNum > 0) &&
    Number(leverage) > 0;

  const submit = () => {
    if (!valid || !market) return;
    onSubmit({
      symbol: market.symbol,
      accountAddress: parentAddress,
      side,
      type,
      priceE9: type === "MARKET" ? "0" : toE9(priceNum),
      quantityE9: toE9(sizeNum),
      leverageE9: toE9(Number(leverage)),
      isIsolated: market.isolatedOnly,
      reduceOnly,
      timeInForce: type === "MARKET" ? "IOC" : "GTT",
      expiresAtMillis: Date.now() + 7 * 24 * 60 * 60 * 1000,
    });
  };

  return (
    <div style={{ marginBottom: 12 }}>
      <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(120px, 1fr))", gap: 8, marginBottom: 8 }}>
        <label style={{ fontSize: 11, opacity: 0.8 }}>
          Market
          <select style={curatorFieldStyle} value={symbol} onChange={(e) => setSymbol(e.target.value)}>
            {tradable.map((m) => (
              <option key={m.symbol} value={m.symbol}>
                {m.symbol}
              </option>
            ))}
          </select>
        </label>
        <label style={{ fontSize: 11, opacity: 0.8 }}>
          Side
          <select style={curatorFieldStyle} value={side} onChange={(e) => setSide(e.target.value as "LONG" | "SHORT")}>
            <option value="LONG">Long</option>
            <option value="SHORT">Short</option>
          </select>
        </label>
        <label style={{ fontSize: 11, opacity: 0.8 }}>
          Type
          <select style={curatorFieldStyle} value={type} onChange={(e) => setType(e.target.value as "LIMIT" | "MARKET")}>
            <option value="LIMIT">Limit</option>
            <option value="MARKET">Market</option>
          </select>
        </label>
        <label style={{ fontSize: 11, opacity: 0.8 }}>
          Leverage
          <input style={curatorFieldStyle} type="number" min="1" value={leverage} onChange={(e) => setLeverage(e.target.value)} />
        </label>
        {type === "LIMIT" && (
          <label style={{ fontSize: 11, opacity: 0.8 }}>
            Price
            <input style={curatorFieldStyle} type="number" min="0" value={price} onChange={(e) => setPrice(e.target.value)} />
          </label>
        )}
        <label style={{ fontSize: 11, opacity: 0.8 }}>
          Size
          <input style={curatorFieldStyle} type="number" min="0" value={size} onChange={(e) => setSize(e.target.value)} />
        </label>
      </div>
      <label style={{ fontSize: 12, display: "flex", gap: 6, alignItems: "center", marginBottom: 8 }}>
        <input type="checkbox" checked={reduceOnly} onChange={(e) => setReduceOnly(e.target.checked)} />
        Reduce-only
      </label>
      <button className="vault-invest__cta" disabled={disabled || !valid} onClick={submit}>
        {side === "LONG" ? "Buy / Long" : "Sell / Short"} {market?.symbol}
      </button>
    </div>
  );
}

function OpenOrders({
  orders,
  busy,
  onCancel,
}: {
  orders: BluefinOpenOrder[];
  busy: boolean;
  onCancel: (symbol: string, hash: string) => void;
}) {
  return (
    <div style={{ marginBottom: 12 }}>
      <div className="vault-card__head" style={{ fontSize: 13 }}>
        Open orders · {orders.length}
      </div>
      {orders.length === 0 ? (
        <div className="vault-prose__muted" style={{ fontSize: 12 }}>No open orders.</div>
      ) : (
        <div className="vault-table">
          <div className="vault-table__scroll">
            <div className="vault-table__head" style={{ gridTemplateColumns: "1fr 0.7fr 1fr 1fr 0.6fr" }}>
              <span>Market</span>
              <span>Side</span>
              <span>Price</span>
              <span>Size</span>
              <span />
            </div>
            {orders.map((o) => (
              <div className="vault-table__row" style={{ gridTemplateColumns: "1fr 0.7fr 1fr 1fr 0.6fr" }} key={o.orderHash}>
                <span>{o.symbol}</span>
                <span>{o.side}</span>
                <span>{fmt(fromE9(o.priceE9))}</span>
                <span>{fmt(fromE9(o.quantityE9), 4)}</span>
                <button className="vault-invest__tab" disabled={busy} onClick={() => onCancel(o.symbol, o.orderHash)}>
                  Cancel
                </button>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

function RecentFills({ trades }: { trades: BluefinTrade[] }) {
  return (
    <div>
      <div className="vault-card__head" style={{ fontSize: 13 }}>
        Recent fills
      </div>
      {trades.length === 0 ? (
        <div className="vault-prose__muted" style={{ fontSize: 12 }}>No recent fills.</div>
      ) : (
        <div className="vault-table">
          <div className="vault-table__scroll">
            <div className="vault-table__head" style={{ gridTemplateColumns: "1fr 0.7fr 1fr 1fr" }}>
              <span>Market</span>
              <span>Side</span>
              <span>Price</span>
              <span>Size</span>
            </div>
            {trades.slice(0, 15).map((t, i) => (
              <div className="vault-table__row" style={{ gridTemplateColumns: "1fr 0.7fr 1fr 1fr" }} key={i}>
                <span>{t.symbol}</span>
                <span>{t.side ?? t.positionSide ?? "—"}</span>
                <span>{fmt(fromE9(t.priceE9))}</span>
                <span>{fmt(fromE9(t.quantityE9), 4)}</span>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
