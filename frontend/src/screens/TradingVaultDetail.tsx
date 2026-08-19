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
  isRiskOff,
  riskStateLabel,
  tokenForCoinType,
  tradingVaultPps,
  tradingVaultTvl,
  trancheTvl,
  type TradingVaultDetail as TradingVaultDetailDto,
  type VaultHoldingPosition,
  type VaultPosition,
  type VaultWaterfall,
} from "../api/tradingVaults";
import {
  useAllowlistedPools,
  useAppraisalPlan,
  useExchangeBm,
  usePendingRequests,
  useSettlement,
  useTradingVault,
  useTradingVaultOnchain,
  useTradingVaultPpsHistory,
  useTradingVaultTrades,
  useVaultPositions,
  useVaultProtocolConfigId,
  useWaterfall,
  type AllowlistedPool,
} from "../api/useTradingVaults";
import { canon, useVaultHoldings, type VaultHolding } from "../api/vaultHoldings";
import { useTradingVaultActions } from "../state/tradingVault";
import { useCoinBalance } from "../api/useCoinBalance";
import {
  DEEPBOOK_ADAPTER_PACKAGE_ID,
  EXCHANGE_ADAPTER_PACKAGE_ID,
  SUPPORTED_TOKENS,
  TRADING_VAULT_PACKAGE_ID,
} from "../config";
import { Address } from "../components/Address";
import { CoverageGauge } from "../components/CoverageGauge";
import { TokenLogo } from "../components/TokenLogo";
import { HowTranchesWork } from "../components/TrancheEducation";
import { TradingVaultPpsChart, type RegimeWindow } from "../components/TradingVaultPpsChart";
import { Toast } from "../components/Toast";
import { VaultLifecycleTimeline } from "../components/VaultLifecycleTimeline";
import { WaterfallExplorer } from "../components/WaterfallExplorer";
import { formatPrice } from "../format";
import { BLUEFIN_TEST_ENABLED, isBluefinTestUsdc } from "../bluefinTest";
import { VaultPositionCard, positionShares } from "../components/VaultPositionCard";
import { BluefinTestFunds } from "./curator/BluefinTestFunds";
import { ExternalVenuePanel } from "./curator/ExternalVenuePanel";
import { curatorFieldStyle } from "./curator/styles";
import { VaultStateBadges, fmtDurationMs, shortHex } from "./TradingVaults";

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
  const waterfallQ = useWaterfall(vault.capitalStructure != null ? vault.vaultId : null);
  // Deduped with SettlementCard's query by key; the timeline wants it too.
  const settlementQ = useSettlement(vault.vaultId, vault.state === "closed");
  const isCurator =
    account?.address != null &&
    normalizeSuiAddress(account.address) === normalizeSuiAddress(vault.curator);
  const tranched = vault.capitalStructure != null;

  // §3.2 regime shading windows. TODO(SO-418): only the CURRENT risk-off
  // window is derivable from the vault DTO (impaired_since_ms / the reset
  // proposal / the live risk state) — historical breach/cure windows need an
  // indexer state-transition series the API doesn't serve yet.
  const regimes: RegimeWindow[] = [];
  if (tranched) {
    if (vault.impairedSinceMs != null) {
      regimes.push({ fromMs: vault.impairedSinceMs, toMs: null, kind: "impaired" });
    }
    if (vault.resetProposal != null) {
      regimes.push({
        fromMs: vault.resetProposal.proposedAtMs,
        toMs: null,
        kind: "reset_pending",
      });
    }
    if (vault.riskState === "coverage_breach") {
      // No breach-start timestamp exists on the DTO; the latest capital sync
      // that observed the breach is the closest honest anchor.
      regimes.push({
        fromMs: waterfallQ.data?.updatedAtMs ?? vault.updatedAtMs,
        toMs: null,
        kind: "coverage_breach",
      });
    }
  }

  return (
    <>
      <CapitalStateBanner vault={vault} symbol={symbol} decimals={token?.decimals ?? null} />
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
            <VaultStateBadges vault={vault} />
          </div>
        </div>
        <div className="vault-stat">
          <div className="vault-stat__label">Share price</div>
          {tranched ? (
            <>
              <div className="vault-stat__val">
                {vault.seniorPps != null ? vault.seniorPps.toFixed(6) : "—"}
                <span className="vault-stat__unit"> sr</span>
              </div>
              <div className="vault-stat__sub">
                {vault.juniorPps != null ? vault.juniorPps.toFixed(6) : "—"} jr · in {symbol}
              </div>
            </>
          ) : (
            <>
              <div className="vault-stat__val">
                {pps != null ? pps.toFixed(6) : "—"}
                <span className="vault-stat__unit"> {symbol}</span>
              </div>
              <div className="vault-stat__sub">latest appraisal</div>
            </>
          )}
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

      {tranched && (
        <TrancheStrip
          vault={vault}
          waterfall={waterfallQ.data ?? null}
          symbol={symbol}
          decimals={token?.decimals ?? null}
        />
      )}

      <div className="vault-grid">
        <div className="vault-grid__main">
          {tranched && waterfallQ.data != null && (
            <WaterfallExplorer
              waterfall={waterfallQ.data}
              symbol={symbol}
              decimals={token?.decimals ?? null}
              termsVersion={vault.termsVersion}
            />
          )}
          {tranched && vault.capitalStructure != null && (
            <div className="vault-card">
              <div className="vault-card__head">Coverage</div>
              <CoverageGauge
                bufferBps={waterfallQ.data?.juniorBufferBps ?? vault.juniorBufferBps}
                targetBps={vault.capitalStructure.targetJuniorBps}
                maintenanceBps={vault.capitalStructure.maintenanceJuniorBps}
                variant="full"
              />
            </div>
          )}
          <TradingVaultPpsChart
            points={ppsHistoryQ.data ?? []}
            loading={ppsHistoryQ.isLoading}
            symbol={symbol}
            tranched={tranched}
            hurdleBpsAnnual={vault.capitalStructure?.seniorHurdleBpsAnnual ?? null}
            regimes={regimes}
          />
          <HoldingsCard vault={vault} />
          <SpotTradesCard vault={vault} />
          <PositionsCard vault={vault} symbol={symbol} decimals={token?.decimals ?? null} />
          <WithdrawQueueCard vault={vault} decimals={token?.decimals ?? null} />
          {vault.state === "closed" && (
            <SettlementCard vault={vault} symbol={symbol} decimals={token?.decimals ?? null} />
          )}
          <ExternalAccountCard vault={vault} symbol={symbol} decimals={token?.decimals ?? null} />
          {isCurator && (
            <CuratorPanel vault={vault} symbol={symbol} decimals={token?.decimals ?? null} />
          )}
          <VaultLifecycleTimeline
            vault={vault}
            points={ppsHistoryQ.data ?? []}
            settlement={settlementQ.data ?? null}
          />
          <TermsCard vault={vault} symbol={symbol} />
        </div>
        <div className="vault-grid__side">
          <UserPanel
            vault={vault}
            waterfall={waterfallQ.data ?? null}
            symbol={symbol}
            decimals={token?.decimals ?? null}
          />
        </div>
      </div>
    </>
  );
}

/**
 * Capital-state banner (SO-418): risk state, curator-commitment breach, and
 * the reset countdown when a proposal is live. Renders nothing while the
 * vault is healthy and unbreached.
 */
function CapitalStateBanner({
  vault,
  symbol,
  decimals,
}: {
  vault: TradingVaultDetailDto;
  symbol: string;
  decimals: number | null;
}) {
  const proposal = vault.resetProposal;
  if (vault.riskState === "healthy" && !vault.curatorCommitmentBreached && proposal == null) {
    return null;
  }
  const toDisplay = (raw: string): string =>
    decimals != null ? formatPrice(Number(raw) / 10 ** decimals, { grouping: true }) : raw;
  const now = Date.now();
  return (
    <div className="dash-alert" role="alert" style={{ marginBottom: 12 }}>
      {vault.riskState !== "healthy" && (
        <div>
          <strong>{riskStateLabel(vault.riskState)}.</strong>{" "}
          {vault.riskState === "coverage_breach" &&
            "The junior buffer is below maintenance: junior withdrawals pause (they stay queued in order), senior withdrawals keep flowing, and the curator can only unwind — not deploy. New senior deposits stop."}
          {vault.riskState === "impaired" &&
            "Junior is wiped and assets are below the senior claim: all ordinary deposits stop; only unwind, repayments, appraisals, and senior exits continue."}
          {vault.riskState === "reset_pending" &&
            "A junior generational reset is proposed. Deposits stop; recapitalization goes through the reset."}
          {vault.impairedSinceMs != null && (
            <span className="vault-prose__muted"> Impaired since {fmtDateTime(vault.impairedSinceMs)}.</span>
          )}
        </div>
      )}
      {vault.curatorCommitmentBreached && (
        <div style={{ marginTop: vault.riskState !== "healthy" ? 6 : 0 }}>
          <strong>Curator commitment breach.</strong> The curator's escrowed
          first-loss commitment is marked below the protocol floor — deployment
          is paused (user exits keep flowing) until it is re-funded.
        </div>
      )}
      {proposal != null && (
        <div style={{ marginTop: 6 }}>
          <strong>Junior reset proposed</strong> (generation {proposal.oldGeneration} →{" "}
          {proposal.oldGeneration + 1}).{" "}
          {proposal.executableAtMs > now
            ? `Executable in ${fmtDurationMs(proposal.executableAtMs - now)}`
            : "Executable now"}
          {" · "}recorded NAV {toDisplay(proposal.recordedNavRaw)} {symbol} vs senior claim{" "}
          {toDisplay(proposal.recordedSeniorClaimRaw)} {symbol} · quoted minimum deposit{" "}
          {toDisplay(proposal.recordedRequiredDepositRaw)} {symbol}.{" "}
          <span className="vault-prose__muted">
            The binding minimum is recomputed at execution; recovery before
            execution cancels the reset. A completed reset permanently wipes
            the old junior generation.
          </span>
        </div>
      )}
    </div>
  );
}

/**
 * Per-tranche stat strip for tranched vaults (SO-418): senior claim, the
 * junior buffer vs its two immutable thresholds, and per-tranche pps/NAV.
 */
function TrancheStrip({
  vault,
  waterfall,
  symbol,
  decimals,
}: {
  vault: TradingVaultDetailDto;
  waterfall: VaultWaterfall | null;
  symbol: string;
  decimals: number | null;
}) {
  const cs = vault.capitalStructure;
  if (cs == null) return null;
  const toDisplay = (raw: string | null): string =>
    raw != null && decimals != null
      ? formatPrice(Number(raw) / 10 ** decimals, { grouping: true })
      : "—";
  const bufferBps = waterfall?.juniorBufferBps ?? vault.juniorBufferBps;
  const bufferTone =
    bufferBps == null
      ? undefined
      : bufferBps < cs.maintenanceJuniorBps
        ? "var(--aqua-down, #e05555)"
        : bufferBps < cs.targetJuniorBps
          ? "#d99a2b"
          : "var(--aqua-up, #1fbf75)";
  return (
    <div className="vault-stats" style={{ marginTop: 12 }}>
      <div className="vault-stat">
        <div className="vault-stat__label">Senior claim</div>
        <div className="vault-stat__val">
          {toDisplay(waterfall?.seniorClaimRaw ?? vault.seniorClaimRaw)}
          <span className="vault-stat__unit"> {symbol}</span>
        </div>
        <div className="vault-stat__sub">
          hurdle {(cs.seniorHurdleBpsAnnual / 100).toFixed(2)}%/yr, priority — not guaranteed
        </div>
      </div>
      <div className="vault-stat">
        <div className="vault-stat__label">Junior buffer</div>
        <div className="vault-stat__val" style={{ color: bufferTone }}>
          {bufferBps != null ? `${(bufferBps / 100).toFixed(2)}%` : "—"}
        </div>
        <div className="vault-stat__sub">
          target {(cs.targetJuniorBps / 100).toFixed(2)}% · maintenance{" "}
          {(cs.maintenanceJuniorBps / 100).toFixed(2)}%
        </div>
      </div>
      <div className="vault-stat">
        <div className="vault-stat__label">Senior</div>
        <div className="vault-stat__val">
          {vault.seniorPps != null ? vault.seniorPps.toFixed(6) : "—"}
          <span className="vault-stat__unit"> {symbol}/share</span>
        </div>
        <div className="vault-stat__sub">
          NAV {trancheTvl(waterfall?.seniorNavRaw ?? vault.seniorNavRaw, decimals) != null
            ? `${formatPrice(trancheTvl(waterfall?.seniorNavRaw ?? vault.seniorNavRaw, decimals) as number, { grouping: true })} ${symbol}`
            : "—"}
        </div>
      </div>
      <div className="vault-stat">
        <div className="vault-stat__label">Junior</div>
        <div className="vault-stat__val">
          {vault.juniorPps != null ? vault.juniorPps.toFixed(6) : "—"}
          <span className="vault-stat__unit"> {symbol}/share</span>
        </div>
        <div className="vault-stat__sub">
          NAV {trancheTvl(waterfall?.juniorNavRaw ?? vault.juniorNavRaw, decimals) != null
            ? `${formatPrice(trancheTvl(waterfall?.juniorNavRaw ?? vault.juniorNavRaw, decimals) as number, { grouping: true })} ${symbol}`
            : "—"} · generation {vault.activeJuniorGeneration}
        </div>
      </div>
    </div>
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
  const [tab, setTab] = useState<"capital" | "external" | "spot" | "exchange" | "assets">(
    "capital",
  );

  return (
    <div className="vault-card">
      <div className="vault-card__head">Curator</div>
      <div className="vault-invest__tabs">
        <button
          className={"vault-invest__tab" + (tab === "capital" ? " is-active" : "")}
          onClick={() => setTab("capital")}
        >
          Capital
        </button>
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
      {tab === "capital" ? (
        <CuratorCapitalPanel
          vault={vault}
          symbol={symbol}
          decimals={decimals}
          actions={actions}
          cfgId={cfgQ.data ?? null}
          plan={planQ.data ?? null}
          planError={planQ.isError ? planQ.error.message : null}
        />
      ) : tab === "external" ? (
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
 * Curator capital ops (SO-418): the escrowed first-loss commitment
 * (fund / release), the junior reset flow (propose / execute), the terminal
 * settlement snapshot + curator-fee claim, and a manual capital sync. All
 * wallet-paid; every appraisal-consuming op composes the same legs as a
 * deposit.
 */
function CuratorCapitalPanel({
  vault,
  symbol,
  decimals,
  actions,
  cfgId,
  plan,
  planError,
}: {
  vault: TradingVaultDetailDto;
  symbol: string;
  decimals: number | null;
  actions: ReturnType<typeof useTradingVaultActions>;
  cfgId: string | null;
  plan: import("../tx/appraisal").AppraisalPlan | null;
  planError: string | null;
}) {
  const [fundAmount, setFundAmount] = useState("");
  const [releaseShares, setReleaseShares] = useState("");
  const [resetAmount, setResetAmount] = useState("");

  const tranched = vault.capitalStructure != null;
  const riskOff = isRiskOff(vault);
  const planBlocked =
    plan == null ? (planError ? `Appraisal unavailable: ${planError}` : "Analyzing vault holdings…") : null;
  const baseBlocked = planBlocked ?? (!cfgId ? "Protocol config unavailable" : null);

  const amountNum = Number(fundAmount) || 0;
  // §7: commitment funding is legal while Open in Healthy/CoverageBreach.
  const fundBlocked =
    baseBlocked ??
    (vault.state !== "open"
      ? "The vault is no longer open"
      : vault.riskStateCode >= 2
        ? "Deposits are blocked while impaired / reset-pending"
        : null);
  // §7: release is blocked risk-off while Open; floor-free while Closing.
  const releaseBlocked =
    baseBlocked ??
    (vault.state === "closed"
      ? "Closed — use the settled-commitment withdrawal"
      : vault.state === "open" && riskOff
        ? "Blocked while risk-off"
        : null);

  const resetProposal = vault.resetProposal;
  const now = Date.now();
  const resetAmountNum = Number(resetAmount) || 0;

  const onFund = () => {
    if (!plan || !cfgId || decimals == null || amountNum <= 0) return;
    actions.depositIntoCommitment({
      plan,
      protocolConfigId: cfgId,
      curatorCapId: vault.curatorCapId,
      amountRaw: BigInt(Math.round(amountNum * 10 ** decimals)),
    });
    setFundAmount("");
  };
  const onRelease = () => {
    if (!plan || !cfgId || decimals == null) return;
    const n = Number(releaseShares) || 0;
    actions.releaseCommitment({
      plan,
      protocolConfigId: cfgId,
      curatorCapId: vault.curatorCapId,
      // Display shares carry the virtual offset; 0 releases everything.
      sharesRaw: n <= 0 ? 0n : BigInt(Math.round(n * 10 ** decimals * SHARE_OFFSET)),
      sender: vault.curator,
    });
    setReleaseShares("");
  };

  return (
    <>
      <div className="vault-card__head" style={{ fontSize: 13 }}>
        First-loss commitment
      </div>
      <div className="vault-kv" style={{ marginBottom: 10 }}>
        <div className="vault-kv__row">
          <span>Status</span>
          <span style={{ color: vault.curatorCommitmentBreached ? "#d99a2b" : undefined }}>
            {vault.curatorCommitmentBreached
              ? "BREACHED — below the protocol floor; deployment is paused until re-funded"
              : "At or above the protocol floor"}
          </span>
        </div>
        <div className="vault-kv__row">
          <span>Commitment tranche</span>
          <span>{tranched ? "junior (first loss)" : "untranched"}</span>
        </div>
      </div>
      <div
        style={{
          display: "grid",
          gridTemplateColumns: "repeat(auto-fit, minmax(140px, 1fr))",
          gap: 10,
          marginBottom: 10,
        }}
      >
        <CuratorField label={`Fund commitment (${symbol})`}>
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
          disabled={!!actions.busy || amountNum <= 0 || decimals == null || fundBlocked != null}
          title={fundBlocked ?? undefined}
          onClick={onFund}
        >
          Fund
        </button>
        <CuratorField label="Release shares (0 = all)">
          <input
            style={curatorFieldStyle}
            type="number"
            min="0"
            placeholder="0"
            value={releaseShares}
            onChange={(e) => setReleaseShares(e.target.value)}
          />
        </CuratorField>
        <button
          className="vault-invest__cta"
          style={{ alignSelf: "end" }}
          disabled={!!actions.busy || decimals == null || releaseBlocked != null}
          title={releaseBlocked ?? undefined}
          onClick={onRelease}
        >
          Release
        </button>
      </div>
      <div className="vault-invest__bal">
        The escrowed commitment is marked at the latest ratio; while open a
        release must leave it at or above the protocol floor.
      </div>

      {tranched && (
        <>
          <div className="vault-card__head" style={{ fontSize: 13, marginTop: 12 }}>
            Junior reset
          </div>
          {resetProposal != null ? (
            <>
              <div className="vault-kv" style={{ marginBottom: 10 }}>
                <div className="vault-kv__row">
                  <span>Executable</span>
                  <span>
                    {resetProposal.executableAtMs > now
                      ? `in ${fmtDurationMs(resetProposal.executableAtMs - now)}`
                      : "now"}
                  </span>
                </div>
                <div className="vault-kv__row">
                  <span>Quoted minimum deposit</span>
                  <span>
                    {decimals != null
                      ? `${formatPrice(Number(resetProposal.recordedRequiredDepositRaw) / 10 ** decimals, { grouping: true })} ${symbol}`
                      : resetProposal.recordedRequiredDepositRaw}
                    <span className="vault-bids__sub"> recomputed at execution</span>
                  </span>
                </div>
              </div>
              <div
                style={{
                  display: "grid",
                  gridTemplateColumns: "repeat(auto-fit, minmax(140px, 1fr))",
                  gap: 10,
                  marginBottom: 10,
                }}
              >
                <CuratorField label={`Reset deposit (${symbol})`}>
                  <input
                    style={curatorFieldStyle}
                    type="number"
                    min="0"
                    placeholder="0.0"
                    value={resetAmount}
                    onChange={(e) => setResetAmount(e.target.value)}
                  />
                </CuratorField>
                <button
                  className="vault-invest__cta"
                  style={{ alignSelf: "end" }}
                  disabled={
                    !!actions.busy ||
                    resetAmountNum <= 0 ||
                    decimals == null ||
                    baseBlocked != null ||
                    resetProposal.executableAtMs > now
                  }
                  title={
                    baseBlocked ??
                    (resetProposal.executableAtMs > now
                      ? "The notice period has not elapsed"
                      : "Permanently wipes the old junior generation")
                  }
                  onClick={() => {
                    if (!plan || !cfgId || decimals == null || resetAmountNum <= 0) return;
                    actions.executeJuniorReset({
                      plan,
                      protocolConfigId: cfgId,
                      amountRaw: BigInt(Math.round(resetAmountNum * 10 ** decimals)),
                      sender: vault.curator,
                    });
                    setResetAmount("");
                  }}
                >
                  Execute reset
                </button>
              </div>
              <div className="vault-invest__bal">
                Executing permanently wipes the old junior generation; the
                deposit first cures the senior deficit, only the excess
                becomes new junior NAV. Recovery before execution cancels the
                proposal automatically.
              </div>
            </>
          ) : (
            <>
              <button
                className="vault-invest__cta"
                disabled={
                  !!actions.busy || baseBlocked != null || vault.riskState !== "impaired"
                }
                title={
                  baseBlocked ??
                  (vault.riskState !== "impaired"
                    ? "Eligible only while impaired (junior wiped, assets below the senior claim)"
                    : "Starts the 7-day public notice period")
                }
                onClick={() => {
                  if (!plan || !cfgId) return;
                  actions.proposeJuniorReset({ plan, protocolConfigId: cfgId });
                }}
              >
                Propose junior reset
              </button>
              <div className="vault-invest__bal">
                Permissionless once impairment is objective; execution needs 7
                days of persistent impairment and 7 days of public notice.
              </div>
            </>
          )}
        </>
      )}

      {vault.state === "closed" && (
        <>
          <div className="vault-card__head" style={{ fontSize: 13, marginTop: 12 }}>
            Settlement
          </div>
          {!vault.settled ? (
            <>
              <button
                className="vault-invest__cta"
                disabled={!!actions.busy || baseBlocked != null}
                title={baseBlocked ?? "One-time — freezes per-tranche entitlements forever"}
                onClick={() => {
                  if (!plan || !cfgId) return;
                  actions.snapshotSettlement({ plan, protocolConfigId: cfgId });
                }}
              >
                Take settlement snapshot
              </button>
              <div className="vault-invest__bal">
                Permissionless: consumes a final complete appraisal, runs the
                waterfall once, and freezes each tranche's entitlement —
                senior first.
              </div>
            </>
          ) : (
            <>
              <button
                className="vault-invest__cta"
                disabled={!!actions.busy}
                onClick={() =>
                  actions.claimSettlementCuratorFees({
                    vaultId: vault.vaultId,
                    curatorCapId: vault.curatorCapId,
                    accountingCoinType: vault.accountingAsset,
                  })
                }
              >
                Claim settlement curator fees
              </button>
              <div className="vault-invest__bal">
                Pays out the performance fees crystallized by settlement
                redemptions so far.
              </div>
            </>
          )}
        </>
      )}

      {vault.state !== "closed" && (
        <div style={{ marginTop: 12 }}>
          <button
            className="vault-invest__tab"
            disabled={!!actions.busy || baseBlocked != null}
            title={baseBlocked ?? "Runs hurdle accrual, the waterfall, and the risk-state test now"}
            onClick={() => {
              if (!plan || !cfgId) return;
              actions.crankCapital({ plan, protocolConfigId: cfgId });
            }}
          >
            Sync capital state now
          </button>
        </div>
      )}
    </>
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
  p: VaultHoldingPosition;
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
function PastPositions({ positions }: { positions: VaultHoldingPosition[] }) {
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
  p: VaultHoldingPosition;
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
      <div className="vault-card__head">Custodied positions · {vault.positions.length}</div>
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
 * Pending withdrawal requests, lane-aware (SO-418): two per-tranche FIFO
 * lanes under one GLOBAL sequence. The next request to pay is the lowest
 * global sequence among payable lane heads; a class-blocked junior lane is
 * greyed with its reason and never stalls senior. The connected recipient
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
  const pendingQ = usePendingRequests(vault.vaultId);
  const requests = pendingQ.data ?? [];
  const allowlist = onchainQ.data?.depositAssets ?? [];
  if (requests.length === 0) return null;

  const me = account?.address != null ? normalizeSuiAddress(account.address) : null;
  // Shares carry the SO-370 virtual offset — display divides it back out.
  const fmtShares = (raw: string) =>
    decimals != null
      ? formatPrice(Number(raw) / (10 ** decimals * SHARE_OFFSET), { grouping: true })
      : raw;
  const tranched = vault.capitalStructure != null;
  const lanes: { label: string; lane: "senior" | "junior" }[] = tranched
    ? [
        { label: "Senior lane", lane: "senior" },
        { label: "Junior lane", lane: "junior" },
      ]
    : [{ label: "Queue", lane: "junior" }];
  // The single next-to-pay head across lanes: lowest global seq among
  // payable requests.
  const nextSeq = requests
    .filter((r) => r.payable)
    .reduce<string | null>(
      (min, r) => (min == null || BigInt(r.globalSeq) < BigInt(min) ? r.globalSeq : min),
      null,
    );
  const blockedReasonText = (reason: string | null) =>
    reason === "junior_lane_blocked"
      ? "junior paused — senior keeps flowing; junior resumes in original order when the breach cures"
      : reason === "wiped_generation"
        ? "wiped generation — settles at zero"
        : null;

  return (
    <div className="vault-card">
      <div className="vault-card__head">Withdrawal queue · {vault.pendingWithdrawals}</div>
      {/* §3.5: the two lanes render side by side (stacking only when the
          card is too narrow); each request carries its GLOBAL sequence tag
          and the single next-to-pay head is highlighted across lanes. */}
      <div
        style={{
          display: "grid",
          gridTemplateColumns: tranched ? "repeat(auto-fit, minmax(300px, 1fr))" : "1fr",
          gap: 12,
          marginBottom: 10,
        }}
      >
      {lanes.map(({ label, lane }) => {
        const mine = requests.filter((r) => r.lane === lane);
        if (mine.length === 0 && !tranched) return null;
        const laneBlocked = mine.some((r) => r.blockedReason === "junior_lane_blocked");
        return (
          <div key={lane} style={{ opacity: laneBlocked ? 0.7 : 1 }}>
            {tranched && (
              <div className="vault-card__head" style={{ fontSize: 12 }}>
                {label} · {mine.length}
                {laneBlocked && (
                  <span
                    className="status-pill is-info"
                    style={{ marginLeft: 8, color: "#d99a2b" }}
                    title="Coverage breach: junior withdrawals pause but stay queued in order."
                  >
                    blocked — coverage breach
                  </span>
                )}
              </div>
            )}
            {laneBlocked && (
              // The plain-language rule, in the open — the pill's tooltip
              // never fires on touch.
              <div className="vault-prose__muted" style={{ fontSize: 11, margin: "4px 0" }}>
                Senior keeps flowing; junior resumes in original order when
                the breach cures.
              </div>
            )}
            {mine.length === 0 ? (
              <div className="vault-prose__muted" style={{ fontSize: 11 }}>
                No pending requests in this lane.
              </div>
            ) : (
              <div className="vault-table">
                <div className="vault-table__scroll">
                  <div
                    className="vault-table__head"
                    style={{ gridTemplateColumns: "0.5fr 1.2fr 1fr 1.4fr 1fr" }}
                  >
                    <span>Seq</span>
                    <span>Recipient</span>
                    <span>Shares</span>
                    <span>Paid in</span>
                    <span>Requested</span>
                  </div>
                  {mine.map((r) => {
                    const reason = blockedReasonText(r.blockedReason);
                    return (
                      <div
                        className="vault-table__row"
                        style={{
                          gridTemplateColumns: "0.5fr 1.2fr 1fr 1.4fr 1fr",
                          alignItems: "center",
                          opacity: r.blockedReason != null ? 0.6 : 1,
                        }}
                        key={r.globalSeq}
                        title={reason ?? undefined}
                      >
                        <span>
                          #{r.globalSeq}
                          {r.globalSeq === nextSeq && (
                            <span className="status-pill is-info" style={{ marginLeft: 4 }}>
                              next
                            </span>
                          )}
                        </span>
                        <span>
                          <Address value={r.recipient} label="Recipient" />
                          {me != null && normalizeSuiAddress(r.recipient) === me && (
                            <span className="vault-bids__sub"> you</span>
                          )}
                        </span>
                        <span>{fmtShares(r.sharesRaw)}</span>
                        {me != null && normalizeSuiAddress(r.recipient) === me ? (
                          <AmendControl
                            vaultId={vault.vaultId}
                            seq={BigInt(r.globalSeq)}
                            current={canon(r.payoutCoinType)}
                            allowlist={allowlist}
                            actions={actions}
                          />
                        ) : (
                          <span title={r.payoutCoinType}>{symbolFor(r.payoutCoinType)}</span>
                        )}
                        <span>{r.requestedAtMs > 0 ? fmtAgo(r.requestedAtMs) : "—"}</span>
                      </div>
                    );
                  })}
                </div>
              </div>
            )}
          </div>
        );
      })}
      </div>
      <div className="vault-card__foot vault-prose__muted">
        {tranched
          ? "Strict FIFO within a lane; across lanes the lowest global sequence among payable heads pays first."
          : "The queue pays out FIFO as the curator frees funds."}
      </div>
      {actions.toast && <Toast message={actions.toast.message} variant={actions.toast.variant} />}
    </div>
  );
}

/**
 * Settlement claim view for a Closed vault (SO-418 §8.7): the frozen
 * per-tranche entitlements, the redeemed-vs-outstanding progress, your
 * redeemable positions, and permissionless settle buttons for outstanding
 * queued requests. Before the snapshot it shows the awaiting state.
 */
function SettlementCard({
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
  const settlementQ = useSettlement(vault.vaultId);
  const positionsQ = useVaultPositions(vault.vaultId, address);
  const pendingQ = usePendingRequests(vault.vaultId);
  const cfgId = cfgQ.data ?? null;

  const s = settlementQ.data ?? null;
  const toDisplay = (raw: string): string =>
    decimals != null ? formatPrice(Number(raw) / 10 ** decimals, { grouping: true }) : raw;
  const fmtSupply = (raw: string): string =>
    decimals != null
      ? formatPrice(Number(raw) / (10 ** decimals * SHARE_OFFSET), { grouping: true })
      : raw;

  if (s == null || !s.settled) {
    return (
      <div className="vault-card">
        <div className="vault-card__head">Settlement</div>
        <div className="vault-card__body vault-prose__muted">
          {settlementQ.isLoading
            ? "Reading settlement state…"
            : "The vault is closed but not yet settled — the one-time settlement snapshot freezes per-tranche entitlements, after which every position redeems directly against the pool."}
        </div>
      </div>
    );
  }

  const positions = positionsQ.data ?? [];
  const pending = pendingQ.data ?? [];
  const redeemed = Number(s.redeemedRaw);
  const outstanding = Number(s.outstandingRaw);
  const progress = redeemed + outstanding > 0 ? redeemed / (redeemed + outstanding) : 1;

  return (
    <div className="vault-card">
      <div className="vault-card__head">Settlement · frozen entitlements</div>
      <div className="vault-kv" style={{ marginBottom: 10 }}>
        <div className="vault-kv__row">
          <span>Final NAV</span>
          <span>
            {toDisplay(s.finalNavRaw)} {symbol}
          </span>
        </div>
        <div className="vault-kv__row">
          <span>Senior pool</span>
          <span>
            {toDisplay(s.seniorPoolRaw)} {symbol} over {fmtSupply(s.seniorSupplyRaw)} shares
          </span>
        </div>
        <div className="vault-kv__row">
          <span>Junior pool</span>
          <span>
            {toDisplay(s.juniorPoolRaw)} {symbol} over {fmtSupply(s.juniorSupplyRaw)} shares
            <span className="vault-bids__sub"> gen {s.activeJuniorGeneration}</span>
          </span>
        </div>
        <div className="vault-kv__row">
          <span>Redeemed</span>
          <span>
            {toDisplay(s.redeemedRaw)} {symbol} · {(progress * 100).toFixed(0)}% of claims
          </span>
        </div>
        <div className="vault-kv__row">
          <span>Snapshot</span>
          <span>{fmtDateTime(s.snapshotAtMs)}</span>
        </div>
      </div>

      {/* §3.9: vault-level redeemed-vs-outstanding progress. */}
      <div
        role="progressbar"
        aria-valuenow={Math.round(progress * 100)}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-label="Settlement claims redeemed"
        style={{
          height: 8,
          borderRadius: 4,
          overflow: "hidden",
          background: "var(--aqua-line, rgba(92,107,122,0.2))",
          marginBottom: 4,
        }}
      >
        <div
          style={{
            width: `${progress * 100}%`,
            height: "100%",
            background: "var(--aqua-up, #1fbf75)",
            transition: "width .3s ease",
          }}
        />
      </div>
      <div className="vault-prose__muted" style={{ fontSize: 11, marginBottom: 10 }}>
        {toDisplay(s.redeemedRaw)} {symbol} redeemed · {toDisplay(s.outstandingRaw)} {symbol}{" "}
        outstanding as perpetual claims on the pool
      </div>

      {address != null && positions.length > 0 && (
        <>
          <div className="vault-card__head" style={{ fontSize: 12 }}>
            Your redeemable positions
          </div>
          <div className="vault-kv" style={{ marginBottom: 10 }}>
            {positions.map((p) => (
              <div className="vault-kv__row" key={p.positionId}>
                <span>
                  {p.tranche} · {fmtSupply(p.sharesRaw)} shares
                  {p.wiped && <span className="vault-bids__sub"> wiped — redeems at zero</span>}
                </span>
                <button
                  className="vault-invest__tab"
                  disabled={!!actions.busy || !cfgId}
                  title={!cfgId ? "Protocol config unavailable" : undefined}
                  onClick={() =>
                    cfgId &&
                    actions.redeemSettledPosition({
                      vaultId: vault.vaultId,
                      protocolConfigId: cfgId,
                      positionId: p.positionId,
                      accountingCoinType: vault.accountingAsset,
                    })
                  }
                >
                  Redeem
                </button>
              </div>
            ))}
          </div>
        </>
      )}

      {pending.length > 0 && (
        <>
          <div className="vault-card__head" style={{ fontSize: 12 }}>
            Outstanding queued requests
          </div>
          <div className="vault-kv" style={{ marginBottom: 10 }}>
            {pending.map((r) => (
              <div className="vault-kv__row" key={r.globalSeq}>
                <span>
                  #{r.globalSeq} · <Address value={r.recipient} label="Recipient" />
                </span>
                <button
                  className="vault-invest__tab"
                  disabled={!!actions.busy || !cfgId}
                  title="Permissionless — order no longer matters once NAV is frozen"
                  onClick={() =>
                    cfgId &&
                    actions.settleQueuedRequest({
                      vaultId: vault.vaultId,
                      protocolConfigId: cfgId,
                      globalSeq: BigInt(r.globalSeq),
                      accountingCoinType: vault.accountingAsset,
                    })
                  }
                >
                  Settle
                </button>
              </div>
            ))}
          </div>
        </>
      )}

      <div className="vault-card__foot vault-prose__muted">
        Unredeemed positions are perpetual claims on the pool — late
        redemption costs nothing but earns nothing further. Settlement pays
        the accounting asset; redemptions are gas-sponsored.
      </div>
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

/**
 * The user side panel (SO-418): deposit form with a tranche selector and a
 * post-deposit buffer preview, and the wallet's positions panel — one card
 * per `VaultPosition` NFT with withdraw / partial-withdraw / merge /
 * transfer-with-disclosure actions. Replaces the v1 single-stake view: a
 * wallet may hold N transferable positions per vault.
 */
function UserPanel({
  vault,
  waterfall,
  symbol,
  decimals,
}: {
  vault: TradingVaultDetailDto;
  waterfall: VaultWaterfall | null;
  symbol: string;
  decimals: number | null;
}) {
  const account = useCurrentAccount();
  const address = account?.address ?? null;
  const actions = useTradingVaultActions();
  const cfgQ = useVaultProtocolConfigId();
  const onchainQ = useTradingVaultOnchain(vault.vaultId);
  const positionsQ = useVaultPositions(vault.vaultId, address);

  const [tab, setTab] = useState<"deposit" | "positions">("deposit");
  const [amount, setAmount] = useState("");
  // SO-370 asset picker over the vault's allowlist; null = accounting asset.
  const [depositAssetSel, setDepositAssetSel] = useState<string | null>(null);
  // SO-418 tranche selector; only rendered on tranched vaults.
  const [trancheSel, setTrancheSel] = useState<"senior" | "junior">("junior");

  const accounting = canon(vault.accountingAsset);
  const allowlist = onchainQ.data?.depositAssets ?? [accounting];
  const depAsset =
    depositAssetSel != null && allowlist.includes(depositAssetSel) ? depositAssetSel : accounting;
  const depToken = tokenForCoinType(depAsset);
  const depDecimals = depAsset === accounting ? decimals : depToken?.decimals ?? null;
  const depSymbol = depAsset === accounting ? symbol : depToken?.ticker ?? shortHex(depAsset);

  const balQ = useCoinBalance(address, depAsset);
  const planQ = useAppraisalPlan(vault, depAsset !== accounting ? depAsset : undefined);

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
  const positions = positionsQ.data ?? [];
  const tranched = vault.capitalStructure != null;
  const trancheCode = tranched ? (trancheSel === "senior" ? 1 : 2) : 0;

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

  // Post-deposit junior-buffer preview (SO-418 §3.6): exact only for
  // accounting-asset deposits (a non-accounting deposit's value depends on
  // the oracle mark at submit).
  let postBufferBps: number | null = null;
  if (
    tranched &&
    waterfall != null &&
    depAsset === accounting &&
    decimals != null &&
    amountNum > 0
  ) {
    const v = amountNum * 10 ** decimals;
    const nav = Number(waterfall.navRaw);
    const junior = Number(waterfall.juniorNavRaw);
    if (nav + v > 0) {
      postBufferBps =
        trancheSel === "senior"
          ? (junior * 10_000) / (nav + v)
          : ((junior + v) * 10_000) / (nav + v);
    }
  }
  // §3.6: shares to be minted at the locked ratio —
  // shares = value × (S_t + O) / (nav_t + 1) — exact only for
  // accounting-asset deposits (non-accounting value depends on the oracle
  // mark at submit). Float is fine for a preview.
  let sharesPreview: number | null = null;
  if (depAsset === accounting && decimals != null && amountNum > 0) {
    const v = amountNum * 10 ** decimals;
    const supplyRaw = tranched
      ? Number(
          (trancheSel === "senior" ? waterfall?.seniorSharesRaw : waterfall?.juniorSharesRaw) ??
            NaN,
        )
      : Number(vault.totalSharesRaw);
    const navTRaw = tranched
      ? Number(
          (trancheSel === "senior" ? waterfall?.seniorNavRaw : waterfall?.juniorNavRaw) ?? NaN,
        )
      : vault.latestNavRaw != null
        ? Number(vault.latestNavRaw)
        : Number(vault.totalSharesRaw) === 0
          ? 0 // genesis: zero book prices at the virtual offset exactly
          : NaN;
    if (Number.isFinite(supplyRaw) && Number.isFinite(navTRaw)) {
      const mintedRaw = (v * (supplyRaw + SHARE_OFFSET)) / (navTRaw + 1);
      sharesPreview = mintedRaw / (10 ** decimals * SHARE_OFFSET);
    }
  }

  // Hard inline block: a senior deposit that would push the buffer below the
  // creator-set target is a guaranteed on-chain abort — never let it sign.
  const seniorBufferBlocked =
    tranched &&
    trancheSel === "senior" &&
    waterfall != null &&
    postBufferBps != null &&
    postBufferBps < waterfall.targetJuniorBps;

  // v2 capital-state deposit gates (§7): impaired / reset-pending block all
  // ordinary deposits; senior additionally requires Healthy.
  const stateGate =
    vault.riskStateCode >= 2
      ? "Deposits are blocked while impaired / reset-pending — recapitalization goes through the junior reset"
      : tranched && trancheSel === "senior" && vault.riskStateCode !== 0
        ? "New senior deposits are blocked outside the Healthy state"
        : null;

  const depositDisabled =
    tab === "deposit" &&
    (appraisalBlocked ||
      vault.depositsPaused ||
      vault.state !== "open" ||
      stateGate != null ||
      seniorBufferBlocked ||
      !cfgId ||
      depDecimals == null);
  const depositTitle = vault.depositsPaused
    ? "Deposits are paused"
    : vault.state !== "open"
      ? "The vault is no longer open for deposits"
      : stateGate != null
        ? stateGate
        : seniorBufferBlocked
          ? `Blocked: this senior deposit would push the junior buffer below the ${((waterfall?.targetJuniorBps ?? 0) / 100).toFixed(2)}% target`
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

  const onDeposit = () => {
    if (amountNum <= 0 || !cfgId || depDecimals == null) return;
    const raw = BigInt(Math.round(amountNum * 10 ** depDecimals));
    if (plan) {
      actions.depositAppraised({
        plan,
        protocolConfigId: cfgId,
        amountRaw: raw,
        trancheCode,
        sender: address,
      });
    } else {
      // Accounting-asset-only fallback (canFallback gates the CTA).
      actions.deposit({
        vaultId: vault.vaultId,
        protocolConfigId: cfgId,
        depositCoinType: vault.accountingAsset,
        amountRaw: raw,
        trancheCode,
        sender: address,
      });
    }
    setAmount("");
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
          className={"vault-invest__tab" + (tab === "positions" ? " is-active" : "")}
          onClick={() => setTab("positions")}
        >
          Your positions{positions.length > 0 ? ` · ${positions.length}` : ""}
        </button>
      </div>

      {tab === "deposit" ? (
        <>
          {tranched && (
            <div style={{ marginBottom: 10 }}>
              <CuratorField label="Tranche">
                <select
                  style={curatorFieldStyle}
                  value={trancheSel}
                  onChange={(e) => setTrancheSel(e.target.value as "senior" | "junior")}
                >
                  <option value="junior">
                    Junior — first loss, residual upside
                  </option>
                  <option value="senior">
                    Senior — priority claim, {((vault.capitalStructure?.seniorHurdleBpsAnnual ?? 0) / 100).toFixed(2)}%/yr hurdle
                  </option>
                </select>
              </CuratorField>
              <div className="vault-prose__muted" style={{ fontSize: 11, marginTop: 4 }}>
                {trancheSel === "senior"
                  ? "The hurdle is a priority claim, not guaranteed yield — senior loses money once junior is exhausted."
                  : "Junior absorbs first loss and owns the residual upside per the vault's immutable upside mode."}{" "}
                <a
                  href="https://github.com/ewitulsk/SuiOptions/blob/staging/docs/trading-vault-v2/disclosures.md"
                  target="_blank"
                  rel="noreferrer"
                >
                  Terms v{vault.termsVersion}
                </a>{" "}
                · <HowTranchesWork compact termsVersion={vault.termsVersion} />
              </div>
            </div>
          )}
          {allowlist.length > 1 && (
            <div style={{ marginBottom: 10 }}>
              <CuratorField label="Deposit asset">
                <select
                  style={curatorFieldStyle}
                  value={depAsset}
                  onChange={(e) => setDepositAssetSel(e.target.value)}
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
              onChange={(e) => setAmount(e.target.value)}
            />
            <span className="vault-invest__unit">{depSymbol}</span>
          </div>
          <div className="vault-invest__bal">
            {balance != null
              ? `${formatPrice(balance, { grouping: true })} ${depSymbol} in wallet`
              : "wallet balance unavailable"}
          </div>

          {(sharesPreview != null || (tranched && postBufferBps != null && waterfall != null)) && (
            <div className="vault-kv" style={{ marginBottom: 10 }}>
              {sharesPreview != null && (
                <div className="vault-kv__row">
                  <span>Shares minted (est.)</span>
                  <span>
                    {formatPrice(sharesPreview, { grouping: true })}
                    {tranched && <span className="vault-bids__sub"> {trancheSel}</span>}
                  </span>
                </div>
              )}
              {tranched && postBufferBps != null && waterfall != null && (
              <div className="vault-kv__row">
                <span>Post-deposit junior buffer</span>
                <span
                  style={{
                    color: seniorBufferBlocked
                      ? "var(--aqua-down, #e05555)"
                      : postBufferBps < waterfall.targetJuniorBps
                        ? "#d99a2b"
                        : "var(--aqua-up, #1fbf75)",
                  }}
                >
                  {(postBufferBps / 100).toFixed(2)}%
                  <span className="vault-bids__sub">
                    {" "}target {(waterfall.targetJuniorBps / 100).toFixed(2)}%
                  </span>
                </span>
              </div>
              )}
            </div>
          )}

          <button
            className="vault-invest__cta"
            disabled={!!actions.busy || amountNum <= 0 || depositDisabled}
            onClick={onDeposit}
            title={depositTitle}
          >
            {actions.busy ? `${actions.busy}…` : `Deposit ${depSymbol}`}
          </button>

          {appraisalBlocked && planError ? (
            <div className="vault-card__foot vault-prose__muted">
              Deposits are blocked until the vault's holdings can be appraised:{" "}
              {planError}. Withdrawal requests still work.
            </div>
          ) : (
            // Why the CTA is dead, in the open — the title attribute never
            // fires on touch.
            depositDisabled &&
            depositTitle && (
              <div className="vault-card__foot vault-prose__muted">{depositTitle}</div>
            )
          )}

          {depAsset !== accounting && !depositDisabled && (
            <div className="vault-card__foot vault-prose__muted">
              {depSymbol} deposits are valued into {symbol} by a live oracle
              attestation and are not gas-sponsored — your wallet pays gas.
              {(onchainQ.data?.entryHaircutBps ?? 0) > 0 &&
                ` An entry haircut of ${((onchainQ.data?.entryHaircutBps ?? 0) / 100).toFixed(2)}% applies.`}
            </div>
          )}
          <div className="vault-card__foot vault-prose__muted">
            Each deposit mints a new transferable position NFT with its own
            lockup and cost basis, delivered to your wallet.
          </div>
        </>
      ) : (
        <PositionsPanel
          vault={vault}
          positions={positions}
          loading={positionsQ.isLoading}
          error={positionsQ.isError ? positionsQ.error.message : null}
          allowlist={allowlist}
          symbol={symbol}
          decimals={decimals}
          actions={actions}
          cfgId={cfgId}
          address={address}
        />
      )}

      {actions.toast && <Toast message={actions.toast.message} variant={actions.toast.variant} />}
    </div>
  );
}

/** The wallet's position list with per-position actions (SO-418). */
function PositionsPanel({
  vault,
  positions,
  loading,
  error,
  allowlist,
  symbol,
  decimals,
  actions,
  cfgId,
  address,
}: {
  vault: TradingVaultDetailDto;
  positions: VaultPosition[];
  loading: boolean;
  error: string | null;
  allowlist: string[];
  symbol: string;
  decimals: number | null;
  actions: ReturnType<typeof useTradingVaultActions>;
  cfgId: string | null;
  address: string;
}) {
  if (loading) {
    return <div className="vault-card__body vault-prose__muted">Reading your positions…</div>;
  }
  if (error != null) {
    return (
      <div className="vault-card__body vault-prose__muted">
        Couldn't read your positions just now — retrying. ({error})
      </div>
    );
  }
  if (positions.length === 0) {
    return (
      <div className="vault-card__body vault-prose__muted">
        You hold no positions in this vault. Each deposit mints a transferable
        position NFT that appears here.
      </div>
    );
  }
  return (
    <>
      {positions.map((p) => (
        <VaultPositionCard key={p.positionId} position={p} symbol={symbol} decimals={decimals}>
          <PositionActions
            vault={vault}
            position={p}
            siblings={positions}
            allowlist={allowlist}
            decimals={decimals}
            actions={actions}
            cfgId={cfgId}
            address={address}
          />
        </VaultPositionCard>
      ))}
      <div className="vault-card__foot vault-prose__muted">
        Positions are freely transferable Sui objects — anyone holding one can
        exit through the queue.{" "}
        <Link to={`/vaults/${vault.vaultId}/positions/${positions[0].positionId}`}>
          Shareable position page
        </Link>
      </div>
    </>
  );
}

/**
 * Per-position actions: withdraw (whole position), partial withdraw
 * (split-then-request), merge into a compatible sibling, transfer with the
 * §3.4 pre-transfer disclosure (value vs basis — the buyer inherits the
 * embedded fee liability), burn for wiped positions, and redeem once the
 * vault is settled.
 */
function PositionActions({
  vault,
  position: p,
  siblings,
  allowlist,
  decimals,
  actions,
  cfgId,
  address,
}: {
  vault: TradingVaultDetailDto;
  position: VaultPosition;
  siblings: VaultPosition[];
  allowlist: string[];
  decimals: number | null;
  actions: ReturnType<typeof useTradingVaultActions>;
  cfgId: string | null;
  address: string;
}) {
  const [mode, setMode] = useState<null | "withdraw" | "merge" | "transfer">(null);
  const [payoutSel, setPayoutSel] = useState<string | null>(null);
  const [partShares, setPartShares] = useState("");
  const [mergeSel, setMergeSel] = useState<string | null>(null);
  const [recipient, setRecipient] = useState("");
  const [disclosureAck, setDisclosureAck] = useState(false);

  const accounting = canon(vault.accountingAsset);
  const payAsset = payoutSel != null && allowlist.includes(payoutSel) ? payoutSel : accounting;
  const now = Date.now();
  const locked = p.lockedUntilMs > now;
  const settled = vault.state === "closed" && vault.settled;
  const busy = !!actions.busy;

  const mergeTargets = siblings.filter(
    (s) =>
      s.positionId !== p.positionId &&
      s.trancheCode === p.trancheCode &&
      s.capitalGeneration === p.capitalGeneration,
  );
  const mergeTarget =
    mergeTargets.find((s) => s.positionId === mergeSel) ?? mergeTargets[0] ?? null;

  const withdrawBlocked = settled
    ? "Settled — redeem against the pool instead"
    : vault.state === "closed"
      ? "Closed — awaiting the settlement snapshot"
      : p.wiped
        ? "Wiped positions have no exit value — burn instead"
        : locked
          ? "Still locked"
          : null;

  const partSharesNum = Number(partShares) || 0;
  const displayShares = decimals != null ? Number(p.sharesRaw) / (10 ** decimals * SHARE_OFFSET) : 0;
  const partValid =
    decimals != null && partSharesNum > 0 && partSharesNum < displayShares;

  const toDisplay = (raw: string | null): string =>
    raw != null && decimals != null
      ? `${formatPrice(Number(raw) / 10 ** decimals, { grouping: true })} ${symbolFor(accounting)}`
      : "—";

  return (
    <div style={{ marginTop: 8 }}>
      <div style={{ display: "flex", gap: 6, flexWrap: "wrap" }}>
        {settled && cfgId != null && (
          <button
            className="vault-invest__tab"
            disabled={busy}
            onClick={() =>
              actions.redeemSettledPosition({
                vaultId: vault.vaultId,
                protocolConfigId: cfgId,
                positionId: p.positionId,
                accountingCoinType: vault.accountingAsset,
              })
            }
          >
            Redeem
          </button>
        )}
        {!settled && !p.wiped && (
          <button
            className={"vault-invest__tab" + (mode === "withdraw" ? " is-active" : "")}
            disabled={busy}
            onClick={() => setMode(mode === "withdraw" ? null : "withdraw")}
          >
            Withdraw
          </button>
        )}
        {mergeTargets.length > 0 && (
          <button
            className={"vault-invest__tab" + (mode === "merge" ? " is-active" : "")}
            disabled={busy}
            onClick={() => setMode(mode === "merge" ? null : "merge")}
          >
            Merge
          </button>
        )}
        <button
          className={"vault-invest__tab" + (mode === "transfer" ? " is-active" : "")}
          disabled={busy}
          onClick={() => {
            setDisclosureAck(false);
            setMode(mode === "transfer" ? null : "transfer");
          }}
        >
          Transfer
        </button>
        {p.wiped && (
          <button
            className="vault-invest__tab"
            disabled={busy}
            title="Destroys the NFT at its permanent zero value"
            onClick={() =>
              actions.burnWipedPosition({ vaultId: vault.vaultId, positionId: p.positionId })
            }
          >
            Burn
          </button>
        )}
      </div>

      {mode === "withdraw" && (
        <div style={{ marginTop: 8 }}>
          {allowlist.length > 1 && (
            <CuratorField label="Paid out in">
              <select
                style={curatorFieldStyle}
                value={payAsset}
                onChange={(e) => setPayoutSel(e.target.value)}
              >
                {allowlist.map((t) => (
                  <option key={t} value={t}>
                    {symbolFor(t)}
                  </option>
                ))}
              </select>
            </CuratorField>
          )}
          <button
            className="vault-invest__cta"
            style={{ marginTop: 8, width: "100%" }}
            disabled={busy || withdrawBlocked != null}
            title={withdrawBlocked ?? "Consumes the whole position into the queue"}
            onClick={() =>
              actions.requestWithdraw({
                vaultId: vault.vaultId,
                positionId: p.positionId,
                payoutCoinType: payAsset,
              })
            }
          >
            Withdraw whole position
          </button>
          <div style={{ display: "flex", gap: 6, marginTop: 8 }}>
            <input
              className="amount__input"
              type="number"
              min="0"
              placeholder={`shares (max ${formatPrice(displayShares, { grouping: true })})`}
              value={partShares}
              onChange={(e) => setPartShares(e.target.value)}
            />
            <button
              className="vault-invest__tab"
              style={{ flex: "0 0 auto" }}
              disabled={busy || withdrawBlocked != null || !partValid}
              title={
                withdrawBlocked ??
                (!partValid
                  ? "Enter a share amount below the position's total"
                  : "Splits the position and queues the split part")
              }
              onClick={() => {
                if (decimals == null) return;
                actions.splitThenWithdraw({
                  vaultId: vault.vaultId,
                  positionId: p.positionId,
                  payoutCoinType: payAsset,
                  sharesRaw: BigInt(Math.round(partSharesNum * 10 ** decimals * SHARE_OFFSET)),
                });
                setPartShares("");
              }}
            >
              Withdraw part
            </button>
          </div>
          {withdrawBlocked != null && (
            <div className="vault-prose__muted" style={{ fontSize: 11, marginTop: 4 }}>
              {withdrawBlocked}
            </div>
          )}
        </div>
      )}

      {mode === "merge" && mergeTarget != null && (
        <div style={{ marginTop: 8, display: "flex", gap: 6, alignItems: "end" }}>
          <CuratorField label="Merge into">
            <select
              style={curatorFieldStyle}
              value={mergeTarget.positionId}
              onChange={(e) => setMergeSel(e.target.value)}
            >
              {mergeTargets.map((s) => (
                <option key={s.positionId} value={s.positionId}>
                  {s.positionId.slice(0, 8)}… · {positionShares(s, decimals)} shares
                </option>
              ))}
            </select>
          </CuratorField>
          <button
            className="vault-invest__tab"
            style={{ flex: "0 0 auto" }}
            disabled={busy}
            title="Shares and basis add; the later lock expiry wins"
            onClick={() =>
              actions.mergePositions({
                intoPositionId: mergeTarget.positionId,
                fromPositionId: p.positionId,
              })
            }
          >
            Merge
          </button>
        </div>
      )}

      {mode === "transfer" && (
        <div style={{ marginTop: 8 }}>
          {/* Pre-transfer disclosure (plan §3.4): value vs basis, shown
              BEFORE any sale/transfer — the buyer inherits the embedded fee
              liability. */}
          <div className="dash-alert" role="note" style={{ marginBottom: 8 }}>
            <div>
              <strong>Before you transfer:</strong> current est. value{" "}
              {toDisplay(p.estimatedValueRaw)} vs on-chain cost basis{" "}
              {toDisplay(p.costBasisRaw)}. The recipient{" "}
              <strong>inherits the embedded fee liability</strong> (
              {toDisplay(p.estimatedFeeRaw)} if exited now) — paying a market
              price does not reset the basis.
              {p.wiped && " This position is WIPED and permanently worthless."}
            </div>
            <label style={{ display: "flex", gap: 6, alignItems: "center", marginTop: 6, fontSize: 11 }}>
              <input
                type="checkbox"
                checked={disclosureAck}
                onChange={(e) => setDisclosureAck(e.target.checked)}
              />
              I understand the value-vs-basis economics of this transfer.
            </label>
          </div>
          <div style={{ display: "flex", gap: 6 }}>
            <input
              className="amount__input"
              type="text"
              placeholder="recipient 0x…"
              value={recipient}
              onChange={(e) => setRecipient(e.target.value)}
            />
            <button
              className="vault-invest__tab"
              style={{ flex: "0 0 auto" }}
              disabled={
                busy ||
                !disclosureAck ||
                !/^0x[0-9a-fA-F]{1,64}$/.test(recipient.trim()) ||
                normalizeSuiAddress(recipient.trim() || "0x0") === normalizeSuiAddress(address)
              }
              onClick={() =>
                actions.transferPosition({
                  positionId: p.positionId,
                  recipient: recipient.trim(),
                })
              }
            >
              Transfer
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
