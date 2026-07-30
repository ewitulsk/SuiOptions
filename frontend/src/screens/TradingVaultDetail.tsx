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
  tokenForCoinType,
  tradingVaultPps,
  tradingVaultTvl,
  type TradingVaultDetail as TradingVaultDetailDto,
  type TradingVaultPosition,
} from "../api/tradingVaults";
import {
  useAllowlistedPools,
  useAppraisalPlan,
  useTradingVault,
  useTradingVaultPpsHistory,
  useTradingVaultStake,
  useTradingVaultTrades,
  useVaultProtocolConfigId,
  type AllowlistedPool,
} from "../api/useTradingVaults";
import { useVaultHoldings, type VaultHolding } from "../api/vaultHoldings";
import { useTradingVaultActions } from "../state/tradingVault";
import { useCoinBalance } from "../api/useCoinBalance";
import { DEEPBOOK_ADAPTER_PACKAGE_ID, TRADING_VAULT_PACKAGE_ID } from "../config";
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
  const token = tokenForCoinType(vault.depositAsset);
  const symbol = token?.ticker ?? shortHex(vault.depositAsset);
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
  const rotation =
    vault.rotationAuthority === 0 ? "Creator" : vault.rotationAuthority === 1 ? "Curator" : "Either";
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
          <span>Rotation authority</span>
          <span>{rotation}</span>
        </div>
        <div className="vault-kv__row">
          <span>Max positions</span>
          <span>{vault.maxPositions}</span>
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
        Deposit asset {symbol} · updated {fmtDateTime(vault.updatedAtMs)}
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
  const [tab, setTab] = useState<"external" | "spot">("external");

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
      ) : (
        <SpotPanel vault={vault} actions={actions} />
      )}
      {/* Staging-only testing affordance (SO-311) — compiled out of mainnet
          and prod builds; see `src/bluefinTest.ts`. */}
      {BLUEFIN_TEST_ENABLED && isBluefinTestUsdc(vault.depositAsset) && (
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
                  {b.coinType === vault.depositAsset && (
                    <span className="vault-bids__sub"> deposit asset</span>
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

  // The card is noise on a vault that has never spot-traded.
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
  const balQ = useCoinBalance(address, vault.depositAsset);
  const planQ = useAppraisalPlan(vault);
  const stakeQ = useTradingVaultStake(vault.vaultId, address);

  const [tab, setTab] = useState<"deposit" | "withdraw">("deposit");
  const [amount, setAmount] = useState("");
  // "Max" fills the exact raw share balance; any manual edit reverts to the
  // parsed input so partial withdrawals round like before.
  const [maxUsed, setMaxUsed] = useState(false);

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
  const balance = decimals != null && balQ.data != null ? Number(balQ.data) / 10 ** decimals : null;
  const cfgId = cfgQ.data ?? null;
  const stake = stakeQ.data ?? null;
  const hasStake = stake != null && stake.shares !== "0";
  const pps = tradingVaultPps(vault);

  // Deposits require a complete appraisal. The composer plans every leg; if
  // planning failed but the vault custodies nothing, the plain two-call PTB
  // still works (the on-chain completeness check backstops us).
  const plan = planQ.data ?? null;
  const planError = planQ.isError ? planQ.error.message : null;
  const canFallback = planError != null && vault.positionCount === 0;
  const appraisalBlocked = plan == null && !canFallback;
  const depositDisabled =
    tab === "deposit" &&
    (appraisalBlocked || vault.depositsPaused || vault.state !== "open" || !cfgId);
  const depositTitle = vault.depositsPaused
    ? "Deposits are paused"
    : vault.state !== "open"
      ? "The vault is no longer open for deposits"
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
  // price, crystallized only at fulfillment.
  let preview: { value: number; fee: number; payout: number } | null = null;
  if (tab === "withdraw" && decimals != null && stake != null && pps != null) {
    const userShares = Number(stake.shares);
    const sharesRaw = maxUsed ? userShares : amountNum * 10 ** decimals;
    if (sharesRaw > 0 && userShares > 0) {
      const value = (sharesRaw * pps) / 10 ** decimals;
      const basis =
        (Number(stake.costBasis) * Math.min(1, sharesRaw / userShares)) / 10 ** decimals;
      const profit = Math.max(0, value - basis);
      const fee = (profit * vault.curatorFeeBps) / 10_000;
      preview = { value, fee, payout: value - fee };
    }
  }

  const now = Date.now();
  const lockedMs =
    stake?.lockedUntilMs != null && stake.lockedUntilMs > now
      ? stake.lockedUntilMs - now
      : null;

  const onMax = () => {
    if (!stake || decimals == null) return;
    setAmount(rawToDecimalString(stake.shares, decimals));
    setMaxUsed(true);
  };

  const onSubmit = () => {
    if (decimals == null || amountNum <= 0) return;
    if (tab === "deposit") {
      if (!cfgId) return;
      const raw = BigInt(Math.round(amountNum * 10 ** decimals));
      if (plan) {
        actions.depositAppraised({ plan, protocolConfigId: cfgId, amountRaw: raw });
      } else {
        actions.deposit({
          vaultId: vault.vaultId,
          protocolConfigId: cfgId,
          depositCoinType: vault.depositAsset,
          amountRaw: raw,
        });
      }
    } else {
      const raw =
        maxUsed && stake ? BigInt(stake.shares) : BigInt(Math.round(amountNum * 10 ** decimals));
      actions.requestWithdraw({ vaultId: vault.vaultId, sharesRaw: raw });
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
            <span>{formatPrice(Number(stake.shares) / 10 ** decimals, { grouping: true })}</span>
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
        <span className="vault-invest__unit">{tab === "deposit" ? symbol : "shares"}</span>
      </div>
      <div className="vault-invest__bal">
        {tab === "deposit"
          ? balance != null
            ? `${formatPrice(balance, { grouping: true })} ${symbol} in wallet`
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
        disabled={!!actions.busy || amountNum <= 0 || decimals == null || depositDisabled}
        onClick={onSubmit}
        title={tab === "deposit" ? depositTitle : undefined}
      >
        {actions.busy
          ? `${actions.busy}…`
          : tab === "deposit"
            ? `Deposit ${symbol}`
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
