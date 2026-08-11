// Curated trading-vault detail (SO-288), route `/vaults/:vaultId`.
//
// Header terms + share price/TVL from the api-service detail endpoint, the
// share-price chart (SO-293), the custodied-positions table, and the user
// panel: deposit (appraisal-composed PTB — values every held asset and
// position so `deposit` sees a complete NAV, SO-289), the wallet's stake,
// and request-withdraw with a fee preview (always available; shares are a
// u128).

import { useState } from "react";
import { Link, useParams } from "react-router-dom";
import { useCurrentAccount } from "@mysten/dapp-kit";
import { normalizeSuiAddress } from "@mysten/sui/utils";

import {
  SHARE_OFFSET,
  tokenForCoinType,
  tradingVaultPps,
  tradingVaultTvl,
  type TradingVaultDetail as TradingVaultDetailDto,
  type TradingVaultPosition,
} from "../api/tradingVaults";
import {
  useAllowlistedPools,
  useAppraisalPlan,
  useExchangeBm,
  useTradingVault,
  useTradingVaultOnchain,
  useTradingVaultPpsHistory,
  useTradingVaultStake,
  useTradingVaultTrades,
  useVaultProtocolConfigId,
  type AllowlistedPool,
} from "../api/useTradingVaults";
import { canon, useVaultHoldings, type VaultHolding } from "../api/vaultHoldings";
import { useTradingVaultActions } from "../state/tradingVault";
import { useCoinBalance } from "../api/useCoinBalance";
import { usePythPrices } from "../api/usePythPrice";
import {
  DEEPBOOK_ADAPTER_PACKAGE_ID,
  EXCHANGE_ADAPTER_PACKAGE_ID,
  SUPPORTED_TOKENS,
  TRADING_VAULT_PACKAGE_ID,
} from "../config";
import { Address } from "../components/Address";
import { TokenLogo } from "../components/TokenLogo";
import { TradingVaultPpsChart } from "../components/TradingVaultPpsChart";
import { Toast } from "../components/Toast";
import { formatPrice } from "../format";
import { BLUEFIN_TEST_ENABLED, isBluefinTestUsdc } from "../bluefinTest";
import { BluefinTestFunds } from "./curator/BluefinTestFunds";
import { ExternalVenuePanel } from "./curator/ExternalVenuePanel";
import { curatorFieldStyle } from "./curator/styles";
import { StateBadge, fmtDurationMs, shortHex } from "./TradingVaults";

function fmtDateTime(ms: number | null | undefined): string {
  if (ms == null || ms <= 0) return "—";
  return new Date(ms).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  });
}

/** Relative age of a timestamp: "just now", "5m ago", "3h ago", "2d ago". */
function fmtAgo(ms: number): string {
  const age = Date.now() - ms;
  if (age < 60_000) return "just now";
  const mins = Math.round(age / 60_000);
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.round(mins / 60);
  if (hours < 48) return `${hours}h ago`;
  return `${Math.round(hours / 24)}d ago`;
}

/** Adapter short-name: the module path after the package address. */
function adapterName(adapter: string): string {
  const parts = adapter.split("::");
  return parts.length > 1 ? parts.slice(1).join("::") : shortHex(adapter);
}

/** Exact raw→display conversion (no float), e.g. ("1500000", 6) → "1.5". */
function rawToDecimalString(raw: string, decimals: number): string {
  const digits = raw.replace(/^0+/, "") || "0";
  if (decimals === 0) return digits;
  const s = digits.padStart(decimals + 1, "0");
  const int = s.slice(0, -decimals);
  const frac = s.slice(-decimals).replace(/0+$/, "");
  return frac ? `${int}.${frac}` : int;
}

export function TradingVaultDetailScreen() {
  const { vaultId } = useParams<{ vaultId: string }>();
  const vaultQ = useTradingVault(vaultId ?? null);

  if (!TRADING_VAULT_PACKAGE_ID) {
    return (
      <div style={{ position: "relative", minHeight: "100%" }}>
        <div className="app__wrap">
          <div className="dash-empty">
            <div className="dash-empty__title">trading vaults unavailable.</div>
            <div className="dash-empty__sub">
              No trading-vault deployment exists on this network.
            </div>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div style={{ position: "relative", minHeight: "100%" }}>
      <div className="app__wrap">
        <div className="vault-detail__bar">
          <Link className="vault-back" to="/vaults">← All vaults</Link>
        </div>
        {vaultQ.isLoading && <div className="vault-note">Loading vault…</div>}
        {vaultQ.isError && (
          <div className="dash-alert" role="alert">
            Couldn't load vault: {vaultQ.error.message}
          </div>
        )}
        {vaultQ.data && <VaultBody vault={vaultQ.data} />}
      </div>
    </div>
  );
}

function VaultBody({ vault }: { vault: TradingVaultDetailDto }) {
  const account = useCurrentAccount();
  const token = tokenForCoinType(vault.accountingAsset);
  const symbol = token?.ticker ?? shortHex(vault.accountingAsset);
  const pps = tradingVaultPps(vault);
  const tvl = tradingVaultTvl(vault, token?.decimals ?? null);
  const ppsHistoryQ = useTradingVaultPpsHistory(vault.vaultId);
  const isCurator =
    account?.address != null &&
    normalizeSuiAddress(account.address) === normalizeSuiAddress(vault.curator);

  return (
    <>
      <div className="vault-stats">
        <div className="vault-stat vault-stat--hero">
          <div className="vault-stat__label">Asset</div>
          <div className="vault-stat__val" style={{ display: "flex", alignItems: "center", gap: 8 }}>
            <TokenLogo
              symbol={token?.ticker}
              className="asset-glyph"
              fallback={<span className="asset-glyph">{symbol[0] ?? "?"}</span>}
            />
            {symbol}
          </div>
          <div className="vault-stat__sub">
            <StateBadge state={vault.state} />
          </div>
        </div>
        <div className="vault-stat">
          <div className="vault-stat__label">Share price</div>
          <div className="vault-stat__val">
            {pps != null ? pps.toFixed(6) : "—"}
            <span className="vault-stat__unit"> {symbol}</span>
          </div>
          <div className="vault-stat__sub">latest appraisal</div>
        </div>
        <div className="vault-stat">
          <div className="vault-stat__label">TVL</div>
          <div className="vault-stat__val">
            {tvl != null ? formatPrice(tvl, { grouping: true }) : "—"}
            <span className="vault-stat__unit"> {symbol}</span>
          </div>
          <div className="vault-stat__sub">shares × share price</div>
        </div>
        <div className="vault-stat">
          <div className="vault-stat__label">Withdrawal queue</div>
          <div className="vault-stat__val">{vault.pendingWithdrawals}</div>
          <div className="vault-stat__sub">pending requests</div>
        </div>
      </div>

      <div className="vault-grid">
        <div className="vault-grid__main">
          <TradingVaultPpsChart
            points={ppsHistoryQ.data ?? []}
            loading={ppsHistoryQ.isLoading}
            symbol={symbol}
          />
          <HoldingsCard vault={vault} />
          <SpotTradesCard vault={vault} />
          <PositionsCard vault={vault} symbol={symbol} decimals={token?.decimals ?? null} />
          <WithdrawQueueCard vault={vault} decimals={token?.decimals ?? null} />
          <ExternalAccountCard vault={vault} symbol={symbol} decimals={token?.decimals ?? null} />
          {isCurator && (
            <CuratorPanel vault={vault} symbol={symbol} decimals={token?.decimals ?? null} />
          )}
          <TermsCard vault={vault} symbol={symbol} />
        </div>
        <div className="vault-grid__side">
          <UserPanel vault={vault} symbol={symbol} decimals={token?.decimals ?? null} />
        </div>
      </div>
    </>
  );
}

function TermsCard({ vault, symbol }: { vault: TradingVaultDetailDto; symbol: string }) {
  return (
    <div className="vault-card">
      <div className="vault-card__head">Terms</div>
      <div className="vault-kv">
        <div className="vault-kv__row">
          <span>Curator</span>
          <Address value={vault.curator} label="Curator" />
        </div>
        <div className="vault-kv__row">
          <span>Creator</span>
          <Address value={vault.creator} label="Creator" />
        </div>
        <div className="vault-kv__row">
          <span>Curator fee</span>
          <span>{(vault.curatorFeeBps / 100).toFixed(2)}% of profit</span>
        </div>
        <div className="vault-kv__row">
          <span>Lockup</span>
          <span>{fmtDurationMs(vault.lockupMs)}</span>
        </div>
        <div className="vault-kv__row">
          <span>Unwind grace</span>
          <span>{fmtDurationMs(vault.unwindGraceMs)}</span>
        </div>
        <div className="vault-kv__row">
          <span>Deposits</span>
          <span>{vault.depositsPaused ? "Paused" : "Open"}</span>
        </div>
      </div>
      <div className="vault-card__foot vault-prose__muted">
        Accounting asset {symbol} · updated {fmtDateTime(vault.updatedAtMs)}
      </div>
    </div>
  );
}

/** External MM account (SO-299): read-only view of the whitelisted external
 * wallet, its outstanding exposure, and the latest keeper-posted equity mark.
 * Renders nothing when the vault has no external account. */
function ExternalAccountCard({
  vault,
  symbol,
  decimals,
}: {
  vault: TradingVaultDetailDto;
  symbol: string;
  decimals: number | null;
}) {
  if (vault.externalAccount == null) return null;

  const toDisplay = (raw: string): string =>
    decimals != null ? formatPrice(Number(raw) / 10 ** decimals, { grouping: true }) : raw;

  return (
    <div className="vault-card">
      <div className="vault-card__head">External account</div>
      <div className="vault-kv">
        <div className="vault-kv__row">
          <span>Account</span>
          <Address value={vault.externalAccount} label="External account" />
        </div>
        <div className="vault-kv__row">
          <span>Outstanding exposure</span>
          <span>
            {toDisplay(vault.externalExposure)} {symbol}
          </span>
        </div>
        <div className="vault-kv__row">
          <span>Latest posted equity</span>
          <span>
            {vault.latestExternalEquity != null
              ? `${toDisplay(vault.latestExternalEquity)} ${symbol}`
              : "—"}
          </span>
        </div>
        <div className="vault-kv__row">
          <span>Equity mark</span>
          <span>
            {vault.externalEquityUpdatedAtMs != null
              ? fmtAgo(vault.externalEquityUpdatedAtMs)
              : "never posted"}
          </span>
        </div>
      </div>
      <div className="vault-card__foot vault-prose__muted">
        Funds released to this account trade off-vault; equity marks are posted
        by the curator's keeper.
      </div>
    </div>
  );
}

// ── curator section (SO-299) ────────────────────────────────────────────────

function CuratorField({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label style={{ fontSize: 11, opacity: 0.8, display: "block" }}>
      {label}
      {children}
    </label>
  );
}

/**
 * Curator-only controls, shown when the connected wallet IS the vault's
 * curator: the hedge panel (budgeted `release_external` + sweep-back recipe)
 * and vault spot trading via the deepbook-adapter taker-swap surface.
 * Curator ops are never gas-sponsored — the curator's wallet pays gas.
 */
function CuratorPanel({
  vault,
  symbol,
  decimals,
}: {
  vault: TradingVaultDetailDto;
  symbol: string;
  decimals: number | null;
}) {
  const actions = useTradingVaultActions();
  const cfgQ = useVaultProtocolConfigId();
  const planQ = useAppraisalPlan(vault);
  const [tab, setTab] = useState<"external" | "spot" | "exchange" | "assets">("external");

  return (
    <div className="vault-card">
      <div className="vault-card__head">Curator</div>
      <div className="vault-invest__tabs">
        <button
          className={"vault-invest__tab" + (tab === "external" ? " is-active" : "")}
          onClick={() => setTab("external")}
        >
          External venue
        </button>
        <button
          className={"vault-invest__tab" + (tab === "spot" ? " is-active" : "")}
          onClick={() => setTab("spot")}
        >
          Spot
        </button>
        <button
          className={"vault-invest__tab" + (tab === "exchange" ? " is-active" : "")}
          onClick={() => setTab("exchange")}
        >
          Exchange
        </button>
        <button
          className={"vault-invest__tab" + (tab === "assets" ? " is-active" : "")}
          onClick={() => setTab("assets")}
        >
          Assets
        </button>
      </div>
      {tab === "external" ? (
        <ExternalVenuePanel
          vault={vault}
          symbol={symbol}
          decimals={decimals}
          actions={actions}
          cfgId={cfgQ.data ?? null}
          plan={planQ.data ?? null}
          planError={planQ.isError ? planQ.error.message : null}
        />
      ) : tab === "spot" ? (
        <SpotPanel vault={vault} actions={actions} />
      ) : tab === "exchange" ? (
        <ExchangePanel vault={vault} actions={actions} />
      ) : (
        <AssetsPanel vault={vault} actions={actions} cfgId={cfgQ.data ?? null} />
      )}
      {/* Staging-only testing affordance (SO-311) — compiled out of mainnet
          and prod builds; see `src/bluefinTest.ts`. */}
      {BLUEFIN_TEST_ENABLED && isBluefinTestUsdc(vault.accountingAsset) && (
        <BluefinTestFunds vault={vault} cfgId={cfgQ.data ?? null} />
      )}
      <div className="vault-card__foot vault-prose__muted">
        Curator transactions are not gas-sponsored — your wallet pays gas.
        Venue setup and sweep run through the co-signing ceremony.
      </div>
      {actions.toast && <Toast message={actions.toast.message} variant={actions.toast.variant} />}
    </div>
  );
}

/**
 * Vault spot trading: curator taker swaps of vault FREE balances against an
 * admin-allowlisted DeepBook pool (`taker_swap_base_for_quote` /
 * `taker_swap_quote_for_base` — no price guardrails by design, `min_out` is
 * the only brake). The custody surface (BalanceManager + resting limit
 * orders) is deferred — see `buildCuratorTakerSwapTx`.
 */
function SpotPanel({
  vault,
  actions,
}: {
  vault: TradingVaultDetailDto;
  actions: ReturnType<typeof useTradingVaultActions>;
}) {
  const poolsQ = useAllowlistedPools(Boolean(DEEPBOOK_ADAPTER_PACKAGE_ID));
  const pools = poolsQ.data ?? [];
  const [poolId, setPoolId] = useState("");
  const [side, setSide] = useState<"sell" | "buy">("sell");
  const [amount, setAmount] = useState("");
  const [minOut, setMinOut] = useState("");

  if (!DEEPBOOK_ADAPTER_PACKAGE_ID) {
    return (
      <div className="vault-card__body vault-prose__muted">
        The deepbook-adapter package is not deployed on this network.
      </div>
    );
  }
  if (poolsQ.isLoading) {
    return <div className="vault-card__body vault-prose__muted">Loading allowlisted pools…</div>;
  }
  if (pools.length === 0) {
    return (
      <div className="vault-card__body vault-prose__muted">
        No DeepBook pools are allowlisted for curator trading. Allowlisting
        (allow_pool) is an admin act.
      </div>
    );
  }

  const pool: AllowlistedPool = pools.find((p) => p.poolId === poolId) ?? pools[0];
  const baseToken = tokenForCoinType(pool.baseType);
  const quoteToken = tokenForCoinType(pool.quoteType);
  const baseSym = baseToken?.ticker ?? shortHex(pool.baseType);
  const quoteSym = quoteToken?.ticker ?? shortHex(pool.quoteType);
  // sell = base in, quote out; buy = quote in, base out.
  const inToken = side === "sell" ? baseToken : quoteToken;
  const outToken = side === "sell" ? quoteToken : baseToken;
  const inSym = side === "sell" ? baseSym : quoteSym;
  const outSym = side === "sell" ? quoteSym : baseSym;

  const amountNum = Number(amount) || 0;
  const minOutNum = Number(minOut);
  const decimalsKnown = inToken != null && outToken != null;
  const valid =
    decimalsKnown && amountNum > 0 && Number.isFinite(minOutNum) && minOutNum >= 0;
  const title = !decimalsKnown
    ? "Pool assets are not in the token catalog"
    : minOut.trim() === ""
      ? "Set Min received — it is the only slippage brake"
      : undefined;

  const onSwap = () => {
    if (!valid || !inToken || !outToken) return;
    actions.spotSwap({
      vaultId: vault.vaultId,
      curatorCapId: vault.curatorCapId,
      poolId: pool.poolId,
      baseType: pool.baseType,
      quoteType: pool.quoteType,
      baseForQuote: side === "sell",
      amountRaw: BigInt(Math.round(amountNum * 10 ** inToken.decimals)),
      minOutRaw: BigInt(Math.round(minOutNum * 10 ** outToken.decimals)),
    });
    setAmount("");
    setMinOut("");
  };

  return (
    <>
      <div
        style={{
          display: "grid",
          gridTemplateColumns: "repeat(auto-fit, minmax(140px, 1fr))",
          gap: 10,
          marginBottom: 10,
        }}
      >
        <CuratorField label="Pool">
          <select
            style={curatorFieldStyle}
            value={pool.poolId}
            onChange={(e) => setPoolId(e.target.value)}
          >
            {pools.map((p) => {
              const b = tokenForCoinType(p.baseType)?.ticker ?? shortHex(p.baseType);
              const q = tokenForCoinType(p.quoteType)?.ticker ?? shortHex(p.quoteType);
              return (
                <option key={p.poolId} value={p.poolId}>
                  {b}/{q}
                </option>
              );
            })}
          </select>
        </CuratorField>
        <CuratorField label="Side">
          <select
            style={curatorFieldStyle}
            value={side}
            onChange={(e) => setSide(e.target.value as "sell" | "buy")}
          >
            <option value="sell">Sell {baseSym} for {quoteSym}</option>
            <option value="buy">Buy {baseSym} with {quoteSym}</option>
          </select>
        </CuratorField>
        <CuratorField label={`Amount (${inSym})`}>
          <input
            style={curatorFieldStyle}
            type="number"
            min="0"
            placeholder="0.0"
            value={amount}
            onChange={(e) => setAmount(e.target.value)}
          />
        </CuratorField>
        <CuratorField label={`Min received (${outSym})`}>
          <input
            style={curatorFieldStyle}
            type="number"
            min="0"
            placeholder="0.0"
            value={minOut}
            onChange={(e) => setMinOut(e.target.value)}
          />
        </CuratorField>
      </div>
      <div className="vault-invest__bal">
        swaps the vault's free {inSym} — no price guardrails, min received is
        the only brake
      </div>
      <button
        className="vault-invest__cta"
        disabled={!!actions.busy || !valid || minOut.trim() === ""}
        onClick={onSwap}
        title={title}
      >
        {actions.busy ? `${actions.busy}…` : `Swap ${inSym} → ${outSym}`}
      </button>
      {title && <div className="vault-card__foot vault-prose__muted">{title}</div>}
    </>
  );
}

/**
 * SO-373 exchange custody: swap vault capital in/out of the hybrid
 * exchange. Creates the vault's cap-owned BalanceManager, funds/defunds it
 * from vault free balances, and delegates order-signing hot keys. A direct
 * custody (SO-372) escrows against vault free balances instead — no
 * funding, signer management only.
 */
function ExchangePanel({
  vault,
  actions,
}: {
  vault: TradingVaultDetailDto;
  actions: ReturnType<typeof useTradingVaultActions>;
}) {
  const holdingsQ = useVaultHoldings(vault);

  if (!EXCHANGE_ADAPTER_PACKAGE_ID) {
    return (
      <div className="vault-card__body vault-prose__muted">
        The exchange-adapter package is not deployed on this network.
      </div>
    );
  }
  const activeCount = vault.positions.filter((p) => p.active).length;
  if (activeCount > 0 && !holdingsQ.data) {
    return (
      <div className="vault-card__body vault-prose__muted">
        {holdingsQ.isError
          ? "Couldn't read the vault's custodied positions just now — retrying."
          : "Reading custodied positions…"}
      </div>
    );
  }

  const custodies = [...(holdingsQ.data?.entries() ?? [])].flatMap(([id, h]) =>
    h.kind === "exchangeCustody" ? [{ custodyId: id, ...h }] : [],
  );
  const hasFunded = custodies.some((c) => !c.direct);

  return (
    <>
      {custodies.map((c) => (
        <ExchangeCustodyCard key={c.custodyId} vault={vault} custody={c} actions={actions} />
      ))}
      {!hasFunded && (
        <>
          <div className="vault-invest__bal">
            {custodies.length === 0
              ? "No exchange custody yet — create one to market-make on the hybrid exchange with vault capital."
              : "Only a direct custody exists — a funded custody warehouses working capital in the manager."}
          </div>
          <button
            className="vault-invest__cta"
            disabled={!!actions.busy}
            onClick={() =>
              actions.initExchangeCustody({
                vaultId: vault.vaultId,
                curatorCapId: vault.curatorCapId,
              })
            }
          >
            {actions.busy ? `${actions.busy}…` : "Create exchange custody"}
          </button>
        </>
      )}
    </>
  );
}

function ExchangeCustodyCard({
  vault,
  custody,
  actions,
}: {
  vault: TradingVaultDetailDto;
  custody: { custodyId: string; bmId: string; assets: string[]; direct: boolean };
  actions: ReturnType<typeof useTradingVaultActions>;
}) {
  const bmQ = useExchangeBm(custody.bmId, custody.assets);
  const [fundType, setFundType] = useState("");
  const [fundAmount, setFundAmount] = useState("");
  const [defundType, setDefundType] = useState("");
  const [defundAmount, setDefundAmount] = useState("");
  const [signer, setSigner] = useState("");

  const ids = {
    vaultId: vault.vaultId,
    curatorCapId: vault.curatorCapId,
    custodyId: custody.custodyId,
    bmId: custody.bmId,
  };
  // Fund draws on vault free balances; defund returns manager holdings.
  const fundable = vault.balances.filter((b) => b.decimals !== null && b.amountRaw !== "0");
  const fund = fundable.find((b) => b.coinType === fundType) ?? fundable[0] ?? null;
  const defundableTypes = custody.assets.filter((t) => tokenForCoinType(t) != null);
  const defund = defundableTypes.includes(defundType)
    ? defundType
    : defundableTypes[0] ?? null;

  const move = (
    kind: "fund" | "defund",
    coinType: string,
    decimals: number,
    amount: string,
  ) => {
    const n = Number(amount);
    if (!(n > 0)) return;
    const params = { ...ids, coinType, amountRaw: BigInt(Math.round(n * 10 ** decimals)) };
    if (kind === "fund") actions.exchangeFund(params);
    else actions.exchangeDefund(params);
  };

  return (
    <div style={{ marginBottom: 12 }}>
      <div className="vault-kv" style={{ marginBottom: 10 }}>
        <div className="vault-kv__row">
          <span>Balance manager</span>
          <Address value={custody.bmId} label="Balance manager" />
        </div>
        {custody.direct ? (
          <div className="vault-kv__row">
            <span>Mode</span>
            <span>direct escrow — orders settle against vault free balances</span>
          </div>
        ) : custody.assets.length > 0 ? (
          custody.assets.map((t) => (
            <div className="vault-kv__row" key={t}>
              <span title={t}>{symbolFor(t)} in manager</span>
              <span>
                {(() => {
                  const raw = bmQ.data?.balances[t];
                  const dec = tokenForCoinType(t)?.decimals;
                  return raw != null && dec != null ? rawToDecimalString(raw, dec) : "—";
                })()}
              </span>
            </div>
          ))
        ) : (
          <div className="vault-kv__row">
            <span>Manager holdings</span>
            <span>empty</span>
          </div>
        )}
      </div>
      {!custody.direct && (
        <div
          style={{
            display: "grid",
            gridTemplateColumns: "repeat(auto-fit, minmax(140px, 1fr))",
            gap: 10,
            marginBottom: 10,
          }}
        >
          <CuratorField label="Fund with">
            <select
              style={curatorFieldStyle}
              value={fund?.coinType ?? ""}
              onChange={(e) => setFundType(e.target.value)}
              disabled={fundable.length === 0}
            >
              {fundable.map((b) => (
                <option key={b.coinType} value={b.coinType}>
                  {b.symbol} — {b.decimals != null ? rawToDecimalString(b.amountRaw, b.decimals) : b.amountRaw} free
                </option>
              ))}
            </select>
          </CuratorField>
          <CuratorField label="Amount">
            <input
              style={curatorFieldStyle}
              type="number"
              min="0"
              placeholder="0.0"
              value={fundAmount}
              onChange={(e) => setFundAmount(e.target.value)}
            />
          </CuratorField>
          <button
            className="vault-invest__cta"
            style={{ alignSelf: "end" }}
            disabled={!!actions.busy || !fund || !(Number(fundAmount) > 0)}
            onClick={() => {
              if (!fund || fund.decimals == null) return;
              move("fund", fund.coinType, fund.decimals, fundAmount);
              setFundAmount("");
            }}
          >
            Fund
          </button>
          <CuratorField label="Defund asset">
            <select
              style={curatorFieldStyle}
              value={defund ?? ""}
              onChange={(e) => setDefundType(e.target.value)}
              disabled={defundableTypes.length === 0}
            >
              {defundableTypes.map((t) => (
                <option key={t} value={t}>
                  {symbolFor(t)}
                </option>
              ))}
            </select>
          </CuratorField>
          <CuratorField label="Amount">
            <input
              style={curatorFieldStyle}
              type="number"
              min="0"
              placeholder="0.0"
              value={defundAmount}
              onChange={(e) => setDefundAmount(e.target.value)}
            />
          </CuratorField>
          <button
            className="vault-invest__cta"
            style={{ alignSelf: "end" }}
            disabled={!!actions.busy || !defund || !(Number(defundAmount) > 0)}
            onClick={() => {
              const dec = defund ? tokenForCoinType(defund)?.decimals : null;
              if (!defund || dec == null) return;
              move("defund", defund, dec, defundAmount);
              setDefundAmount("");
            }}
          >
            Defund
          </button>
        </div>
      )}
      <div className="vault-kv" style={{ marginBottom: 10 }}>
        {(bmQ.data?.signers ?? []).map((s) => (
          <div className="vault-kv__row" key={s}>
            <Address value={s} label="Order signer" />
            <button
              className="vault-invest__tab"
              disabled={!!actions.busy}
              onClick={() => actions.exchangeRemoveSigner({ ...ids, signer: s })}
            >
              Remove
            </button>
          </div>
        ))}
      </div>
      <div
        style={{
          display: "grid",
          gridTemplateColumns: "repeat(auto-fit, minmax(140px, 1fr))",
          gap: 10,
          marginBottom: 10,
        }}
      >
        <CuratorField label="Delegate order signer">
          <input
            style={curatorFieldStyle}
            type="text"
            placeholder="0x…"
            value={signer}
            onChange={(e) => setSigner(e.target.value)}
          />
        </CuratorField>
        <button
          className="vault-invest__cta"
          style={{ alignSelf: "end" }}
          disabled={!!actions.busy || !/^0x[0-9a-fA-F]{1,64}$/.test(signer.trim())}
          onClick={() => {
            actions.exchangeAddSigner({ ...ids, signer: signer.trim() });
            setSigner("");
          }}
        >
          Add signer
        </button>
      </div>
      <div className="vault-invest__bal">
        {custody.direct
          ? "Delegated keys sign maker orders escrowed by vault free balances; removing a key voids its resting orders."
          : "Funded capital backs maker orders signed by delegated keys; defunding to zero voids the book's resting orders at fill time."}
      </div>
    </div>
  );
}

/**
 * SO-370 multi-asset management: the deposit/payout allowlist
 * (`add_deposit_asset` / `remove_deposit_asset`, capped on-chain) and the
 * entry/exit haircut dampers on non-accounting flows (`set_haircuts`).
 */
function AssetsPanel({
  vault,
  actions,
  cfgId,
}: {
  vault: TradingVaultDetailDto;
  actions: ReturnType<typeof useTradingVaultActions>;
  cfgId: string | null;
}) {
  const onchainQ = useTradingVaultOnchain(vault.vaultId);
  const [addType, setAddType] = useState("");
  // null = untouched, prefilled from chain below.
  const [entryBps, setEntryBps] = useState<string | null>(null);
  const [exitBps, setExitBps] = useState<string | null>(null);

  if (onchainQ.isLoading) {
    return <div className="vault-card__body vault-prose__muted">Reading the vault's allowlist…</div>;
  }
  if (onchainQ.isError || !onchainQ.data) {
    return (
      <div className="vault-card__body vault-prose__muted">
        Couldn't read the vault's allowlist just now — retrying.
      </div>
    );
  }

  const accounting = canon(vault.accountingAsset);
  const allowlist = onchainQ.data.depositAssets;
  const addable = SUPPORTED_TOKENS.filter((t) => {
    if (!t.enabled) return false;
    try {
      return !allowlist.includes(canon(t.coinType));
    } catch {
      return false;
    }
  });
  const add = addable.find((t) => t.coinType === addType) ?? addable[0] ?? null;

  const entry = entryBps ?? String(onchainQ.data.entryHaircutBps);
  const exit = exitBps ?? String(onchainQ.data.exitHaircutBps);
  const entryNum = Number(entry);
  const exitNum = Number(exit);
  // MAX_HAIRCUT_BPS on-chain is 500; mirror it so a doomed tx never submits.
  const haircutsValid =
    Number.isInteger(entryNum) && entryNum >= 0 && entryNum <= 500 &&
    Number.isInteger(exitNum) && exitNum >= 0 && exitNum <= 500;
  const haircutsDirty =
    entryNum !== onchainQ.data.entryHaircutBps || exitNum !== onchainQ.data.exitHaircutBps;

  return (
    <>
      <div className="vault-kv" style={{ marginBottom: 10 }}>
        {allowlist.map((t) => (
          <div className="vault-kv__row" key={t}>
            <span title={t}>
              {symbolFor(t)}
              {t === accounting && <span className="vault-bids__sub"> accounting asset</span>}
            </span>
            {t !== accounting && (
              <button
                className="vault-invest__tab"
                disabled={!!actions.busy}
                onClick={() =>
                  actions.removeDepositAsset({
                    vaultId: vault.vaultId,
                    curatorCapId: vault.curatorCapId,
                    coinType: t,
                  })
                }
              >
                Remove
              </button>
            )}
          </div>
        ))}
      </div>
      {add != null ? (
        <div
          style={{
            display: "grid",
            gridTemplateColumns: "repeat(auto-fit, minmax(140px, 1fr))",
            gap: 10,
            marginBottom: 10,
          }}
        >
          <CuratorField label="Allow deposits in">
            <select
              style={curatorFieldStyle}
              value={add.coinType}
              onChange={(e) => setAddType(e.target.value)}
            >
              {addable.map((t) => (
                <option key={t.coinType} value={t.coinType}>
                  {t.ticker} — {t.name}
                </option>
              ))}
            </select>
          </CuratorField>
          <button
            className="vault-invest__cta"
            style={{ alignSelf: "end" }}
            disabled={!!actions.busy || !cfgId}
            title={!cfgId ? "Protocol config unavailable" : undefined}
            onClick={() =>
              cfgId &&
              actions.addDepositAsset({
                vaultId: vault.vaultId,
                curatorCapId: vault.curatorCapId,
                protocolConfigId: cfgId,
                coinType: add.coinType,
              })
            }
          >
            Add {add.ticker}
          </button>
        </div>
      ) : (
        <div className="vault-invest__bal">Every catalogued token is already allowlisted.</div>
      )}
      <div
        style={{
          display: "grid",
          gridTemplateColumns: "repeat(auto-fit, minmax(140px, 1fr))",
          gap: 10,
          marginBottom: 10,
        }}
      >
        <CuratorField label="Entry haircut (bps)">
          <input
            style={curatorFieldStyle}
            type="number"
            min="0"
            max="500"
            step="1"
            value={entry}
            onChange={(e) => setEntryBps(e.target.value)}
          />
        </CuratorField>
        <CuratorField label="Exit haircut (bps)">
          <input
            style={curatorFieldStyle}
            type="number"
            min="0"
            max="500"
            step="1"
            value={exit}
            onChange={(e) => setExitBps(e.target.value)}
          />
        </CuratorField>
        <button
          className="vault-invest__cta"
          style={{ alignSelf: "end" }}
          disabled={!!actions.busy || !haircutsValid || !haircutsDirty}
          onClick={() =>
            actions.setHaircuts({
              vaultId: vault.vaultId,
              curatorCapId: vault.curatorCapId,
              entryBps: entryNum,
              exitBps: exitNum,
            })
          }
        >
          Set haircuts
        </button>
      </div>
      <div className="vault-invest__bal">
        Haircuts damp oracle arbitrage on non-accounting deposits and payouts
        (max 500 bps each). Every allowlisted asset the vault holds becomes a
        mandatory appraisal leg — keep the list small.
      </div>
    </>
  );
}

/** Short display symbol for a canonical coin type. */
function symbolFor(coinType: string): string {
  return tokenForCoinType(coinType)?.ticker ?? shortHex(coinType);
}

function fmtExpiry(ms: number): string {
  return new Date(ms).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

/** Two-line "what is this position" cell: main label + muted detail. */
function HoldingLabel({
  main,
  sub,
  glyphSymbol,
  badge,
  title,
}: {
  main: React.ReactNode;
  sub: React.ReactNode;
  glyphSymbol?: string;
  badge?: string;
  title: string;
}) {
  return (
    <span style={{ display: "flex", alignItems: "center", gap: 8, minWidth: 0 }} title={title}>
      {glyphSymbol != null && (
        <TokenLogo
          symbol={glyphSymbol}
          className="asset-glyph asset-glyph--sm"
          fallback={<span className="asset-glyph asset-glyph--sm">{glyphSymbol[0] ?? "?"}</span>}
        />
      )}
      <span style={{ minWidth: 0 }}>
        <span style={{ display: "block" }}>
          {main}
          {badge != null && (
            <span className="status-pill is-info" style={{ marginLeft: 6 }}>
              {badge}
            </span>
          )}
        </span>
        <span className="vault-bids__sub">{sub}</span>
      </span>
    </span>
  );
}

/**
 * One active position, described from the classified chain objects. Until
 * the holdings query resolves (or when a position couldn't be classified)
 * the row falls back to the read-model adapter name.
 */
function HoldingSummary({
  p,
  holding,
}: {
  p: TradingVaultPosition;
  holding: VaultHolding | undefined;
}) {
  if (holding == null) {
    return <span title={p.positionId}>{adapterName(p.adapter)}</span>;
  }
  switch (holding.kind) {
    case "custody": {
      const pools = holding.pools.map(
        (pl) => `${symbolFor(pl.baseType)}/${symbolFor(pl.quoteType)}`,
      );
      return (
        <HoldingLabel
          title={p.positionId}
          main="DeepBook custody"
          sub={
            (holding.assets.length > 0
              ? `holds ${holding.assets.map(symbolFor).join(", ")}`
              : "no tracked assets") + (pools.length > 0 ? ` · pools ${pools.join(", ")}` : "")
          }
        />
      );
    }
    case "exchangeCustody":
      return (
        <HoldingLabel
          title={p.positionId}
          main="Exchange custody"
          sub={
            holding.direct
              ? "direct escrow"
              : holding.assets.length > 0
                ? `holds ${holding.assets.map(symbolFor).join(", ")}`
                : "no tracked assets"
          }
        />
      );
    case "rfq":
      return (
        <HoldingLabel
          title={p.positionId}
          main="RFQ ticket"
          sub={`escrow ${symbolFor(holding.escrowType)}`}
        />
      );
    case "option": {
      const sym = holding.bucket?.assetSymbol ?? symbolFor(holding.underlying);
      const isPut = holding.bucket?.isPut ?? holding.isPut;
      const strike = holding.bucket?.strike ?? null;
      return (
        <HoldingLabel
          title={p.positionId}
          glyphSymbol={sym}
          main={`${sym} · ${isPut ? "put" : "call"}${strike != null ? ` · $${formatPrice(strike)}` : ""}`}
          badge={holding.viaVaultMm ? "via vault_mm" : undefined}
          sub={
            holding.bucket != null
              ? `written · expires ${fmtExpiry(holding.bucket.expiryMs)}`
              : "written option position"
          }
        />
      );
    }
    case "optionCoin": {
      const sym = holding.bucket?.assetSymbol ?? symbolFor(holding.coinType);
      const b = holding.bucket;
      return (
        <HoldingLabel
          title={p.positionId}
          glyphSymbol={sym}
          main={
            b != null
              ? `${sym} · ${b.isPut ? "put" : "call"}${b.strike != null ? ` · $${formatPrice(b.strike)}` : ""}`
              : sym
          }
          sub={
            b != null
              ? `held option coin · expires ${fmtExpiry(b.expiryMs)}`
              : "held option coin"
          }
        />
      );
    }
  }
}

/** Closed positions keep the read-model rendering — the chain objects are
 * gone, so adapter + timestamps is all there is. Collapsed by default. */
function PastPositions({ positions }: { positions: TradingVaultPosition[] }) {
  const [open, setOpen] = useState(false);
  return (
    <div style={{ marginTop: 8 }}>
      <button
        className="vault-invest__tab"
        style={{ width: "100%" }}
        onClick={() => setOpen((o) => !o)}
      >
        Past positions · {positions.length} {open ? "▾" : "▸"}
      </button>
      {open && (
        <div className="vault-table">
          <div className="vault-table__scroll">
            <div
              className="vault-table__head"
              style={{ gridTemplateColumns: "1.4fr 0.7fr 1fr 1fr" }}
            >
              <span>Adapter</span>
              <span>Status</span>
              <span>Stored</span>
              <span>Removed</span>
            </div>
            {positions.map((p) => (
              <div
                className="vault-table__row"
                style={{ gridTemplateColumns: "1.4fr 0.7fr 1fr 1fr" }}
                key={p.positionId}
              >
                <span title={p.positionId}>{adapterName(p.adapter)}</span>
                <span>closed</span>
                <span>{fmtDateTime(p.storedAtMs)}</span>
                <span>{fmtDateTime(p.removedAtMs)}</span>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

/** Latest per-position appraisal mark (SO-304): value in deposit-asset
 * units + a subtle "as of" line. "—" until the position is first appraised. */
function PositionValue({
  p,
  symbol,
  decimals,
}: {
  p: TradingVaultPosition;
  symbol: string;
  decimals: number | null;
}) {
  if (p.lastValueRaw == null) return <span>—</span>;
  const value =
    decimals != null
      ? `${formatPrice(Number(p.lastValueRaw) / 10 ** decimals, { grouping: true })} ${symbol}`
      : p.lastValueRaw;
  return (
    <span>
      <span style={{ display: "block" }}>{value}</span>
      {p.lastAppraisedAtMs != null && (
        <span className="vault-bids__sub">as of {fmtDateTime(p.lastAppraisedAtMs)}</span>
      )}
    </span>
  );
}

/**
 * Display amount for a raw u64 in an asset's atomic units. Exact — holdings
 * are the balance itself, not a price, so no significant-figure rounding and
 * no float (a u64 balance can exceed 2^53). Falls back to the raw integer
 * when the asset isn't in the catalog, so an uncatalogued holding still shows
 * a real number rather than "—".
 */
function fmtAmount(amountRaw: string, decimals: number | null): string {
  if (decimals == null) return amountRaw;
  return rawToDecimalString(amountRaw, decimals);
}

/**
 * Free balances the vault holds outside custody (SO-313). A curator spot
 * trade moves value between these and never mints a position, so without
 * this card the trade is invisible on the page.
 */
function HoldingsCard({ vault }: { vault: TradingVaultDetailDto }) {
  // Zero-balance assets drop their on-chain field, so anything the API
  // returns is non-zero; filter defensively anyway.
  const held = vault.balances.filter((b) => b.amountRaw !== "0");

  return (
    <div className="vault-card">
      <div className="vault-card__head">Holdings · {held.length}</div>
      {vault.balancesStale ? (
        <div className="vault-card__body vault-prose__muted">
          Couldn't read the vault's balances just now — retrying.
        </div>
      ) : held.length === 0 ? (
        <div className="vault-card__body vault-prose__muted">
          The vault holds no free balances.
        </div>
      ) : (
        <div className="vault-table">
          <div className="vault-table__scroll">
            <div className="vault-table__head" style={{ gridTemplateColumns: "1.4fr 1fr" }}>
              <span>Asset</span>
              <span>Amount</span>
            </div>
            {held.map((b) => (
              <div
                className="vault-table__row"
                style={{ gridTemplateColumns: "1.4fr 1fr" }}
                key={b.coinType}
              >
                <span title={b.coinType}>
                  {b.symbol}
                  {b.coinType === vault.accountingAsset && (
                    <span className="vault-bids__sub"> accounting asset</span>
                  )}
                </span>
                <span>{fmtAmount(b.amountRaw, b.decimals)}</span>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

/**
 * Curator spot trades against allowlisted DeepBook pools (SO-313). The event
 * carries the pool id and a direction flag only, so the pool allowlist
 * supplies the two coin types; an unresolvable pool degrades to raw amounts.
 */
function SpotTradesCard({ vault }: { vault: TradingVaultDetailDto }) {
  const tradesQ = useTradingVaultTrades(vault.vaultId);
  const trades = tradesQ.data ?? [];
  const poolsQ = useAllowlistedPools(Boolean(DEEPBOOK_ADAPTER_PACKAGE_ID) && trades.length > 0);
  const pools = poolsQ.data ?? [];

  // A failed read must not look like "never traded" — that is the same
  // invisibility SO-313 exists to fix, one layer up.
  if (tradesQ.isError) {
    return (
      <div className="vault-card">
        <div className="vault-card__head">Spot trades</div>
        <div className="vault-card__body vault-prose__muted">
          Couldn't read this vault's trades just now — retrying.
        </div>
      </div>
    );
  }
  // The card is noise on a vault that has genuinely never spot-traded.
  if (trades.length === 0) return null;

  return (
    <div className="vault-card">
      <div className="vault-card__head">Spot trades · {trades.length}</div>
      <div className="vault-table">
        <div className="vault-table__scroll">
          <div className="vault-table__head" style={{ gridTemplateColumns: "1.6fr 1.6fr 1fr 1fr" }}>
            <span>Sold</span>
            <span>Bought</span>
            <span>When</span>
            <span>Tx</span>
          </div>
          {trades.map((t, i) => {
            const pool = pools.find((p) => p.poolId === t.poolId) ?? null;
            // `baseForQuote` — the vault sold the pool's base for its quote.
            const inType = pool ? (t.baseForQuote ? pool.baseType : pool.quoteType) : null;
            const outType = pool ? (t.baseForQuote ? pool.quoteType : pool.baseType) : null;
            const leg = (raw: string, coinType: string | null) =>
              coinType == null
                ? raw
                : `${fmtAmount(raw, tokenForCoinType(coinType)?.decimals ?? null)} ${symbolFor(coinType)}`;
            // `unswapped` came back unfilled, so it was never actually sold.
            const soldRaw = (BigInt(t.amountIn) - BigInt(t.unswapped)).toString();
            return (
              <div
                className="vault-table__row"
                style={{ gridTemplateColumns: "1.6fr 1.6fr 1fr 1fr" }}
                // A single PTB can carry more than one taker swap, so the
                // digest alone isn't unique.
                key={`${t.txDigest}-${i}`}
              >
                <span title={pool ? undefined : `pool ${t.poolId}`}>{leg(soldRaw, inType)}</span>
                <span>{leg(t.amountOut, outType)}</span>
                <span title={fmtDateTime(t.timestampMs)}>{fmtAgo(t.timestampMs)}</span>
                <Address value={t.txDigest} label="Transaction" />
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}

function PositionsCard({
  vault,
  symbol,
  decimals,
}: {
  vault: TradingVaultDetailDto;
  symbol: string;
  decimals: number | null;
}) {
  const holdingsQ = useVaultHoldings(vault);
  const holdings = holdingsQ.data ?? null;
  const active = vault.positions.filter((p) => p.active);
  const past = vault.positions.filter((p) => !p.active);

  return (
    <div className="vault-card">
      <div className="vault-card__head">Positions · {vault.positions.length}</div>
      {vault.positions.length === 0 ? (
        <div className="vault-card__body vault-prose__muted">
          No positions are custodied. Assets the vault holds outside custody —
          including anything the curator has spot-traded into — are listed
          under Holdings.
        </div>
      ) : (
        <>
          {active.length > 0 ? (
            <div className="vault-table">
              <div className="vault-table__scroll">
                <div
                  className="vault-table__head"
                  style={{ gridTemplateColumns: "2.4fr 1fr 1fr" }}
                >
                  <span>Position</span>
                  <span>Value</span>
                  <span>Stored</span>
                </div>
                {active.map((p) => (
                  <div
                    className="vault-table__row"
                    style={{ gridTemplateColumns: "2.4fr 1fr 1fr" }}
                    key={p.positionId}
                  >
                    <HoldingSummary p={p} holding={holdings?.get(p.positionId)} />
                    <PositionValue p={p} symbol={symbol} decimals={decimals} />
                    <span>{fmtDateTime(p.storedAtMs)}</span>
                  </div>
                ))}
              </div>
            </div>
          ) : (
            <div className="vault-card__body vault-prose__muted">No active positions.</div>
          )}
          {past.length > 0 && <PastPositions positions={past} />}
        </>
      )}
    </div>
  );
}

/**
 * Pending withdrawal requests (SO-370), read from the vault object's FIFO
 * queue. Each row names its requested payout asset; the connected recipient
 * can re-point their own pending request (`amend_payout_asset`) — the
 * unwedge lever when the vault can't source the asset they asked for.
 */
function WithdrawQueueCard({
  vault,
  decimals,
}: {
  vault: TradingVaultDetailDto;
  decimals: number | null;
}) {
  const account = useCurrentAccount();
  const actions = useTradingVaultActions();
  const onchainQ = useTradingVaultOnchain(vault.vaultId);
  const requests = onchainQ.data?.requests ?? [];
  const allowlist = onchainQ.data?.depositAssets ?? [];
  if (requests.length === 0) return null;

  const me = account?.address != null ? normalizeSuiAddress(account.address) : null;
  // Shares carry the SO-370 virtual offset — display divides it back out.
  const fmtShares = (raw: string) =>
    decimals != null
      ? formatPrice(Number(raw) / (10 ** decimals * SHARE_OFFSET), { grouping: true })
      : raw;

  return (
    <div className="vault-card">
      <div className="vault-card__head">Withdrawal queue · {vault.pendingWithdrawals}</div>
      <div className="vault-table">
        <div className="vault-table__scroll">
          <div className="vault-table__head" style={{ gridTemplateColumns: "1.2fr 1fr 1.4fr 1fr" }}>
            <span>Recipient</span>
            <span>Shares</span>
            <span>Paid in</span>
            <span>Requested</span>
          </div>
          {requests.map((r) => (
            <div
              className="vault-table__row"
              style={{ gridTemplateColumns: "1.2fr 1fr 1.4fr 1fr", alignItems: "center" }}
              key={String(r.seq)}
            >
              <span>
                <Address value={r.recipient} label="Recipient" />
                {me != null && normalizeSuiAddress(r.recipient) === me && (
                  <span className="vault-bids__sub"> you</span>
                )}
              </span>
              <span>{fmtShares(r.shares)}</span>
              {me != null && normalizeSuiAddress(r.recipient) === me ? (
                <AmendControl
                  vaultId={vault.vaultId}
                  seq={r.seq}
                  current={r.payoutAsset}
                  allowlist={allowlist}
                  actions={actions}
                />
              ) : (
                <span title={r.payoutAsset}>{symbolFor(r.payoutAsset)}</span>
              )}
              <span>{r.requestedAtMs != null ? fmtAgo(r.requestedAtMs) : "—"}</span>
            </div>
          ))}
        </div>
      </div>
      {requests.length < vault.pendingWithdrawals && (
        <div className="vault-card__foot vault-prose__muted">
          Showing the first {requests.length} of {vault.pendingWithdrawals} pending requests —
          the queue pays out FIFO.
        </div>
      )}
      {actions.toast && <Toast message={actions.toast.message} variant={actions.toast.variant} />}
    </div>
  );
}

/** Payout-asset picker + amend CTA for the connected recipient's request. */
function AmendControl({
  vaultId,
  seq,
  current,
  allowlist,
  actions,
}: {
  vaultId: string;
  seq: bigint;
  current: string;
  allowlist: string[];
  actions: ReturnType<typeof useTradingVaultActions>;
}) {
  const [next, setNext] = useState<string | null>(null);
  const options = allowlist.includes(current) ? allowlist : [current, ...allowlist];
  const sel = next != null && options.includes(next) ? next : current;
  return (
    <span style={{ display: "flex", gap: 6, alignItems: "center" }}>
      <select style={curatorFieldStyle} value={sel} onChange={(e) => setNext(e.target.value)}>
        {options.map((t) => (
          <option key={t} value={t}>
            {symbolFor(t)}
          </option>
        ))}
      </select>
      {sel !== current && (
        <button
          className="vault-invest__tab"
          style={{ flex: "0 0 auto" }}
          disabled={!!actions.busy}
          onClick={() => actions.amendPayoutAsset({ vaultId, seq, payoutCoinType: sel })}
          title="Re-point this request's payout asset"
        >
          Amend
        </button>
      )}
    </span>
  );
}

function UserPanel({
  vault,
  symbol,
  decimals,
}: {
  vault: TradingVaultDetailDto;
  symbol: string;
  decimals: number | null;
}) {
  const account = useCurrentAccount();
  const address = account?.address ?? null;
  const actions = useTradingVaultActions();
  const cfgQ = useVaultProtocolConfigId();
  const onchainQ = useTradingVaultOnchain(vault.vaultId);
  const stakeQ = useTradingVaultStake(vault.vaultId, address);

  const [tab, setTab] = useState<"deposit" | "withdraw">("deposit");
  const [amount, setAmount] = useState("");
  // "Max" fills the exact raw share balance; any manual edit reverts to the
  // parsed input so partial withdrawals round like before.
  const [maxUsed, setMaxUsed] = useState(false);
  // SO-370 asset pickers over the vault's allowlist; null = accounting asset.
  const [depositAssetSel, setDepositAssetSel] = useState<string | null>(null);
  const [payoutAssetSel, setPayoutAssetSel] = useState<string | null>(null);

  const accounting = canon(vault.accountingAsset);
  const allowlist = onchainQ.data?.depositAssets ?? [accounting];
  const depAsset =
    depositAssetSel != null && allowlist.includes(depositAssetSel) ? depositAssetSel : accounting;
  const payAsset =
    payoutAssetSel != null && allowlist.includes(payoutAssetSel) ? payoutAssetSel : accounting;
  const depToken = tokenForCoinType(depAsset);
  const depDecimals = depAsset === accounting ? decimals : depToken?.decimals ?? null;
  const depSymbol = depAsset === accounting ? symbol : depToken?.ticker ?? shortHex(depAsset);
  const paySymbol = payAsset === accounting ? symbol : symbolFor(payAsset);

  const balQ = useCoinBalance(address, depAsset);
  const planQ = useAppraisalPlan(vault, depAsset !== accounting ? depAsset : undefined);
  // USD marks for both legs, to estimate a non-accounting payout in its own
  // units (`usePythPrices` keys off the joined symbols, so the inline array
  // is fine). Empty for accounting payouts — no conversion needed.
  const payTicker = payAsset !== accounting ? tokenForCoinType(payAsset)?.ticker ?? null : null;
  const accTicker = tokenForCoinType(vault.accountingAsset)?.ticker ?? null;
  const marks = usePythPrices(payTicker && accTicker ? [payTicker, accTicker] : []);

  if (!address) {
    return (
      <div className="vault-card vault-invest">
        <div className="vault-card__head">Invest</div>
        <div className="vault-card__body vault-prose__muted">
          Connect a wallet to deposit into this vault.
        </div>
      </div>
    );
  }

  const amountNum = Number(amount) || 0;
  const balance =
    depDecimals != null && balQ.data != null ? Number(balQ.data) / 10 ** depDecimals : null;
  const cfgId = cfgQ.data ?? null;
  const stake = stakeQ.data ?? null;
  const hasStake = stake != null && stake.shares !== "0";
  const pps = tradingVaultPps(vault);

  // Deposits require a complete appraisal. The composer plans every leg; if
  // planning failed but the vault custodies nothing, the plain two-call PTB
  // still works for the accounting asset (the on-chain completeness check
  // backstops us). Non-accounting deposits always need the composed plan —
  // their attestation rides in it.
  const plan = planQ.data ?? null;
  const planError = planQ.isError ? planQ.error.message : null;
  const canFallback =
    planError != null && vault.positionCount === 0 && depAsset === accounting;
  const appraisalBlocked = plan == null && !canFallback;
  const depositDisabled =
    tab === "deposit" &&
    (appraisalBlocked ||
      vault.depositsPaused ||
      vault.state !== "open" ||
      !cfgId ||
      depDecimals == null);
  const depositTitle = vault.depositsPaused
    ? "Deposits are paused"
    : vault.state !== "open"
      ? "The vault is no longer open for deposits"
      : depDecimals == null
        ? "This asset is not in the token catalog"
        : !cfgId
          ? cfgQ.isLoading
            ? "Resolving protocol config…"
            : "Protocol config not found for this deployment"
          : appraisalBlocked
            ? planError
              ? `Deposit unavailable: ${planError}`
              : "Analyzing vault holdings…"
            : undefined;

  // Withdrawal fee preview: profit = max(0, value − basis), fee = profit ×
  // curatorFeeBps/10⁴, payout = value − fee. Pro-rata basis like the
  // contract; all display-unit floats — an estimate at the current share
  // price, crystallized only at fulfillment. Raw shares carry the SO-370
  // virtual offset; the displayed pps carries the matching ×1e6, so the
  // offset must divide back out of raw-share × pps products.
  let preview: { value: number; fee: number; payout: number } | null = null;
  if (tab === "withdraw" && decimals != null && stake != null && pps != null) {
    const userShares = Number(stake.shares);
    const sharesRaw = maxUsed ? userShares : amountNum * 10 ** decimals * SHARE_OFFSET;
    if (sharesRaw > 0 && userShares > 0) {
      const value = (sharesRaw * pps) / (10 ** decimals * SHARE_OFFSET);
      const basis =
        (Number(stake.costBasis) * Math.min(1, sharesRaw / userShares)) / 10 ** decimals;
      const profit = Math.max(0, value - basis);
      const fee = (profit * vault.curatorFeeBps) / 10_000;
      preview = { value, fee, payout: value - fee };
    }
  }

  // Vault free inventory of the chosen payout asset (SO-370): the estimated
  // payout is crossed over USD marks for non-accounting assets (skipped when
  // either mark is missing) — an unsourceable payout waits on the curator.
  let freeInventory: { display: number; short: boolean } | null = null;
  if (tab === "withdraw") {
    const bal = vault.balances.find((b) => {
      try {
        return canon(b.coinType) === payAsset;
      } catch {
        return false;
      }
    });
    const payDecimals = bal?.decimals ?? tokenForCoinType(payAsset)?.decimals ?? null;
    if (payDecimals != null && (bal != null || !vault.balancesStale)) {
      const free = bal != null ? Number(bal.amountRaw) / 10 ** payDecimals : 0;
      let est: number | null = null;
      if (preview != null) {
        if (payAsset === accounting) est = preview.payout;
        else {
          const accMark = accTicker ? marks[accTicker]?.price ?? null : null;
          const payMark = payTicker ? marks[payTicker]?.price ?? null : null;
          if (accMark != null && payMark != null && payMark > 0) {
            est = (preview.payout * accMark) / payMark;
          }
        }
      }
      freeInventory = { display: free, short: est != null && free < est };
    }
  }

  const now = Date.now();
  const lockedMs =
    stake?.lockedUntilMs != null && stake.lockedUntilMs > now
      ? stake.lockedUntilMs - now
      : null;

  const onMax = () => {
    if (!stake || decimals == null) return;
    // Raw shares carry the virtual offset — 6 extra display decimals.
    setAmount(rawToDecimalString(stake.shares, decimals + 6));
    setMaxUsed(true);
  };

  const onSubmit = () => {
    if (amountNum <= 0) return;
    if (tab === "deposit") {
      if (!cfgId || depDecimals == null) return;
      const raw = BigInt(Math.round(amountNum * 10 ** depDecimals));
      if (plan) {
        actions.depositAppraised({ plan, protocolConfigId: cfgId, amountRaw: raw });
      } else {
        // Accounting-asset-only fallback (canFallback gates the CTA).
        actions.deposit({
          vaultId: vault.vaultId,
          protocolConfigId: cfgId,
          depositCoinType: vault.accountingAsset,
          amountRaw: raw,
        });
      }
    } else {
      if (decimals == null) return;
      const raw =
        maxUsed && stake
          ? BigInt(stake.shares)
          : BigInt(Math.round(amountNum * 10 ** decimals)) * BigInt(SHARE_OFFSET);
      actions.requestWithdraw({ vaultId: vault.vaultId, sharesRaw: raw, payoutCoinType: payAsset });
    }
    setAmount("");
    setMaxUsed(false);
  };

  return (
    <div className="vault-card vault-invest">
      <div className="vault-invest__tabs">
        <button
          className={"vault-invest__tab" + (tab === "deposit" ? " is-active" : "")}
          onClick={() => setTab("deposit")}
        >
          Deposit
        </button>
        <button
          className={"vault-invest__tab" + (tab === "withdraw" ? " is-active" : "")}
          onClick={() => setTab("withdraw")}
        >
          Withdraw
        </button>
      </div>

      {hasStake && decimals != null && (
        <div className="vault-kv" style={{ marginBottom: 10 }}>
          <div className="vault-kv__row">
            <span>Your shares</span>
            <span>
              {formatPrice(Number(stake.shares) / (10 ** decimals * SHARE_OFFSET), {
                grouping: true,
              })}
            </span>
          </div>
          <div className="vault-kv__row">
            <span>Cost basis</span>
            <span>
              {formatPrice(Number(stake.costBasis) / 10 ** decimals, { grouping: true })} {symbol}
            </span>
          </div>
          <div className="vault-kv__row">
            <span>Est. value</span>
            <span>
              {stake.estimatedValue != null
                ? `${formatPrice(Number(stake.estimatedValue) / 10 ** decimals, { grouping: true })} ${symbol}`
                : "—"}
            </span>
          </div>
          <div className="vault-kv__row">
            <span>Lockup</span>
            <span>{lockedMs != null ? `unlocks in ${fmtDurationMs(lockedMs)}` : "unlocked"}</span>
          </div>
        </div>
      )}

      {allowlist.length > 1 && (
        <div style={{ marginBottom: 10 }}>
          <CuratorField label={tab === "deposit" ? "Deposit asset" : "Paid out in"}>
            <select
              style={curatorFieldStyle}
              value={tab === "deposit" ? depAsset : payAsset}
              onChange={(e) =>
                tab === "deposit"
                  ? setDepositAssetSel(e.target.value)
                  : setPayoutAssetSel(e.target.value)
              }
            >
              {allowlist.map((t) => (
                <option key={t} value={t}>
                  {t === accounting ? `${symbol} — accounting asset` : symbolFor(t)}
                </option>
              ))}
            </select>
          </CuratorField>
        </div>
      )}
      <div className="vault-invest__field">
        <input
          className="amount__input"
          type="number"
          min="0"
          placeholder="0.0"
          value={amount}
          onChange={(e) => {
            setAmount(e.target.value);
            setMaxUsed(false);
          }}
        />
        {tab === "withdraw" && (
          <button
            className="vault-invest__tab"
            style={{ flex: "0 0 auto" }}
            onClick={onMax}
            disabled={!hasStake || decimals == null}
            title="Withdraw your full share balance"
          >
            Max
          </button>
        )}
        <span className="vault-invest__unit">{tab === "deposit" ? depSymbol : "shares"}</span>
      </div>
      <div className="vault-invest__bal">
        {tab === "deposit"
          ? balance != null
            ? `${formatPrice(balance, { grouping: true })} ${depSymbol} in wallet`
            : "wallet balance unavailable"
          : "queued FIFO — paid out as the curator frees funds"}
      </div>

      {preview && (
        <div className="vault-kv" style={{ marginBottom: 10 }}>
          <div className="vault-kv__row">
            <span>Est. value</span>
            <span>{formatPrice(preview.value, { grouping: true })} {symbol}</span>
          </div>
          <div className="vault-kv__row">
            <span>Curator fee ({(vault.curatorFeeBps / 100).toFixed(2)}% of profit)</span>
            <span>{formatPrice(preview.fee, { grouping: true })} {symbol}</span>
          </div>
          <div className="vault-kv__row">
            <span>Est. payout</span>
            <span>{formatPrice(preview.payout, { grouping: true })} {symbol}</span>
          </div>
        </div>
      )}

      <button
        className="vault-invest__cta"
        disabled={
          !!actions.busy ||
          amountNum <= 0 ||
          (tab === "deposit" ? depositDisabled : decimals == null)
        }
        onClick={onSubmit}
        title={tab === "deposit" ? depositTitle : undefined}
      >
        {actions.busy
          ? `${actions.busy}…`
          : tab === "deposit"
            ? `Deposit ${depSymbol}`
            : "Request withdrawal"}
      </button>

      {tab === "deposit" && appraisalBlocked && planError ? (
        <div className="vault-card__foot vault-prose__muted">
          Deposits are blocked until the vault's holdings can be appraised:{" "}
          {planError}. Withdrawal requests still work.
        </div>
      ) : (
        // Why the CTA is dead, in the open — the title attribute never fires
        // on touch.
        depositDisabled &&
        depositTitle && (
          <div className="vault-card__foot vault-prose__muted">{depositTitle}</div>
        )
      )}

      {tab === "deposit" && depAsset !== accounting && !depositDisabled && (
        <div className="vault-card__foot vault-prose__muted">
          {depSymbol} deposits are valued into {symbol} by a live oracle
          attestation and are not gas-sponsored — your wallet pays gas.
          {(onchainQ.data?.entryHaircutBps ?? 0) > 0 &&
            ` An entry haircut of ${((onchainQ.data?.entryHaircutBps ?? 0) / 100).toFixed(2)}% applies.`}
        </div>
      )}

      {tab === "withdraw" && freeInventory && (
        <div className="vault-card__foot vault-prose__muted">
          Vault free {paySymbol}: {formatPrice(freeInventory.display, { grouping: true })}.
          {freeInventory.short &&
            ` Less than the estimated payout — fulfillment may be delayed while the curator frees ${paySymbol}.`}
          {payAsset !== accounting &&
            (onchainQ.data?.exitHaircutBps ?? 0) > 0 &&
            ` An exit haircut of ${((onchainQ.data?.exitHaircutBps ?? 0) / 100).toFixed(2)}% applies to ${paySymbol} payouts.`}
        </div>
      )}

      {tab === "withdraw" && preview && (
        <div className="vault-card__foot vault-prose__muted">
          Estimated at the current share price — the final value, fee, and
          payout crystallize when the withdrawal is fulfilled.
        </div>
      )}

      {actions.toast && <Toast message={actions.toast.message} variant={actions.toast.variant} />}
    </div>
  );
}
