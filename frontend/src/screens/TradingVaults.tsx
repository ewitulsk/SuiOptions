// Curated trading-vault list (SO-288).
//
// Reads the api-service `/trading-vaults` endpoint and renders every vault as
// a row linking into its detail (`/vaults/:vaultId`), plus a collapsible
// "Create vault" form driving `vault::create_vault<T>` through the shared
// submit path. Renders an "unavailable" empty state on networks with no
// trading-vault deployment.

import { useMemo, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useCurrentAccount } from "@mysten/dapp-kit";

import {
  riskStateLabel,
  tokenForCoinType,
  trancheTvl,
  tradingVaultPps,
  tradingVaultTvl,
  type TradingVault,
} from "../api/tradingVaults";
import { useTradingVaults, useVaultProtocolConfigId } from "../api/useTradingVaults";
import { useTradingVaultActions } from "../state/tradingVault";
import { UNTRANCHED_CAPITAL } from "../tx/tradingVault";
import { BLUEFIN_TEST_ENABLED, BLUEFIN_TEST_USDC } from "../bluefinTest";
import { SUPPORTED_TOKENS, TRADING_VAULT_OBJECTS, TRADING_VAULT_PACKAGE_ID } from "../config";
import { Address } from "../components/Address";
import { CoverageGauge } from "../components/CoverageGauge";
import { TokenLogo } from "../components/TokenLogo";
import { Toast } from "../components/Toast";
import { formatPrice } from "../format";
import { curatorFieldStyle } from "./curator/styles";

/** Abbreviate a 0x id/address for dense tables: 0x1234…cdef. */
export function shortHex(s: string): string {
  return s.length > 12 ? `${s.slice(0, 6)}…${s.slice(-4)}` : s;
}

/** Humanize a millisecond duration: "none", "36h", "7d", "30d". */
export function fmtDurationMs(ms: number | null | undefined): string {
  if (ms == null) return "—";
  if (ms <= 0) return "none";
  const hours = ms / 3_600_000;
  if (hours < 48) return `${Math.round(hours)}h`;
  return `${Math.round(hours / 24)}d`;
}

/** Colored open/closing/closed pill, reusing the vault head badge style. */
export function StateBadge({ state }: { state: TradingVault["state"] }) {
  const color =
    state === "open" ? "var(--aqua-up, #1fbf75)" : state === "closing" ? "#d99a2b" : "var(--aqua-ink-3)";
  return (
    <span
      className="vault-head__badge"
      style={{ color, borderColor: "currentcolor", textTransform: "capitalize" }}
    >
      {state}
    </span>
  );
}

/**
 * The full v2 state badge set (SO-418): lifecycle (open/closing/closed) ×
 * capital risk state × settled. The risk badge only renders when it says
 * something (non-healthy or a curator-commitment breach); a closed vault
 * shows whether its settlement snapshot has run.
 */
export function VaultStateBadges({ vault }: { vault: TradingVault }) {
  const riskColor =
    vault.riskState === "coverage_breach"
      ? "#d99a2b"
      : vault.riskState === "impaired"
        ? "var(--aqua-down, #e05555)"
        : vault.riskState === "reset_pending"
          ? "var(--aqua-down, #e05555)"
          : "var(--aqua-up, #1fbf75)";
  return (
    <span style={{ display: "inline-flex", gap: 4, flexWrap: "wrap", alignItems: "center" }}>
      <StateBadge state={vault.state} />
      {vault.state !== "closed" && vault.riskState !== "healthy" && (
        <span
          className="vault-head__badge"
          style={{ color: riskColor, borderColor: "currentcolor" }}
          title="Capital risk state — deployment is paused while risk-off"
        >
          {riskStateLabel(vault.riskState)}
        </span>
      )}
      {vault.state !== "closed" && vault.curatorCommitmentBreached && (
        <span
          className="vault-head__badge"
          style={{ color: "#d99a2b", borderColor: "currentcolor" }}
          title="The curator's escrowed commitment is below the protocol floor — deployment is paused until re-funded"
        >
          Commitment breach
        </span>
      )}
      {vault.state === "closed" && (
        <span
          className="vault-head__badge"
          style={{ color: "var(--aqua-ink-3)", borderColor: "currentcolor" }}
          title={
            vault.settled
              ? "Entitlements are frozen — positions redeem against the settlement pool"
              : "Awaiting the one-time settlement snapshot"
          }
        >
          {vault.settled ? "Settled" : "Awaiting settlement"}
        </span>
      )}
    </span>
  );
}

// Column template shared by the head and rows (vault-table's default grid is
// the covered-call 5-column layout, so override inline).
const GRID: React.CSSProperties = {
  gridTemplateColumns: "1.3fr 0.8fr 1fr 1.1fr 0.9fr 0.7fr 0.8fr 1fr",
};

const VAULT_STATES = ["open", "closing", "closed"] as const;
type VaultState = TradingVault["state"];

export function TradingVaults() {
  const vaultsQ = useTradingVaults();
  const navigate = useNavigate();
  const [filtersOpen, setFiltersOpen] = useState(false);
  // Closed vaults are hidden by default — dozens of dead smoke/test vaults
  // would otherwise bury the live ones.
  const [stateFilter, setStateFilter] = useState<Record<VaultState, boolean>>({
    open: true,
    closing: true,
    closed: false,
  });

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

  const vaults = vaultsQ.data ?? [];
  const visible = vaults.filter((v) => stateFilter[v.state] ?? true);

  return (
    <div style={{ position: "relative", minHeight: "100%" }}>
      <div className="app__wrap">
        <div className="vault-browser__head">
          <span className="vault-head__badge">Trading vaults</span>
          <span className="vault-head__tag">
            Deposit into a curator-managed vault; exit anytime via the FIFO queue.
          </span>
        </div>

        {vaultsQ.isLoading && <div className="vault-note">Loading vaults…</div>}
        {vaultsQ.isError && (
          <div className="dash-alert" role="alert">
            Couldn't load trading vaults: {vaultsQ.error.message}
          </div>
        )}
        {vaultsQ.data && vaults.length === 0 && (
          <div className="dash-empty">
            <div className="dash-empty__title">no vaults yet.</div>
            <div className="dash-empty__sub">
              No trading vault has been created on this network — be the first below.
            </div>
          </div>
        )}

        {vaults.length > 0 && (
          <div className="vault-card">
            <div className="vault-card__head">
              All vaults
              <button
                className="vault-howto__trigger"
                style={{ marginLeft: "auto" }}
                onClick={() => setFiltersOpen((o) => !o)}
                aria-expanded={filtersOpen}
              >
                {filtersOpen ? "Hide filters" : "Filters"}
              </button>
            </div>
            {filtersOpen && (
              <div
                style={{ display: "flex", gap: 8, alignItems: "center", margin: "10px 0" }}
                role="group"
                aria-label="Filter vaults by state"
              >
                <span className="vault-prose__muted" style={{ fontSize: 11 }}>
                  State:
                </span>
                {VAULT_STATES.map((s) => {
                  const on = stateFilter[s];
                  const count = vaults.filter((v) => v.state === s).length;
                  return (
                    <button
                      key={s}
                      className="vault-head__badge"
                      style={{
                        cursor: "pointer",
                        textTransform: "capitalize",
                        opacity: on ? 1 : 0.4,
                        borderColor: "currentcolor",
                        color: on ? undefined : "var(--aqua-ink-3)",
                      }}
                      aria-pressed={on}
                      onClick={() => setStateFilter((f) => ({ ...f, [s]: !f[s] }))}
                    >
                      {s} ({count})
                    </button>
                  );
                })}
              </div>
            )}
            {visible.length === 0 ? (
              <div className="vault-note">
                All {vaults.length} vaults are hidden by the current filters.
              </div>
            ) : (
              <div className="vault-table">
                <div className="vault-table__head" style={GRID}>
                  <span>Asset</span>
                  <span>State</span>
                  <span>Share price</span>
                  <span>TVL</span>
                  <span>Pending w/d</span>
                  <span>Fee</span>
                  <span>Lockup</span>
                  <span>Curator</span>
                </div>
                {visible.map((v) => (
                  <TradingVaultRow
                    key={v.vaultId}
                    vault={v}
                    onOpen={() => navigate(`/vaults/${v.vaultId}`)}
                  />
                ))}
              </div>
            )}
          </div>
        )}

        <CreateVaultCard />
      </div>
    </div>
  );
}

/** Stacked senior/junior mini-rows for a tranched vault's pps/TVL cells. */
function TranchePair({ senior, junior }: { senior: string; junior: string }) {
  return (
    <span>
      <span style={{ display: "block" }}>
        <span className="vault-bids__sub">sr </span>
        {senior}
      </span>
      <span style={{ display: "block" }}>
        <span className="vault-bids__sub">jr </span>
        {junior}
      </span>
    </span>
  );
}

function TradingVaultRow({ vault, onOpen }: { vault: TradingVault; onOpen: () => void }) {
  const token = tokenForCoinType(vault.accountingAsset);
  const symbol =
    token?.ticker ?? shortHex(vault.accountingAsset.split("::")[0] ?? vault.accountingAsset);
  const pps = tradingVaultPps(vault);
  const tvl = tradingVaultTvl(vault, token?.decimals ?? null);
  const tranched = vault.capitalStructure != null;
  const seniorTvl = trancheTvl(vault.seniorNavRaw, token?.decimals ?? null);
  const juniorTvl = trancheTvl(vault.juniorNavRaw, token?.decimals ?? null);
  // Tap-vs-drag guard: these rows live inside a horizontal scroller on phones,
  // where a swipe would otherwise land as a navigation.
  const downAt = useRef<{ x: number; y: number } | null>(null);
  return (
    <div
      className="vault-table__row"
      style={{ ...GRID, cursor: "pointer", alignItems: "center" }}
      onPointerDown={(e) => {
        downAt.current = { x: e.clientX, y: e.clientY };
      }}
      onClick={(e) => {
        const from = downAt.current;
        downAt.current = null;
        if (from && Math.hypot(e.clientX - from.x, e.clientY - from.y) > 10) return;
        onOpen();
      }}
      role="link"
      tabIndex={0}
      onKeyDown={(e) => {
        if (e.key === "Enter") onOpen();
      }}
    >
      <span style={{ display: "flex", alignItems: "center", gap: 8 }}>
        <TokenLogo
          symbol={token?.ticker}
          className="asset-glyph"
          fallback={<span className="asset-glyph">{symbol[0] ?? "?"}</span>}
        />
        {symbol}
      </span>
      <span>
        <VaultStateBadges vault={vault} />
        {/* §3.3 compact coverage gauge: junior buffer vs the vault's two
            immutable thresholds, at a glance on the list. */}
        {tranched && vault.capitalStructure != null && (
          <span style={{ display: "block", marginTop: 4 }}>
            <CoverageGauge
              bufferBps={vault.juniorBufferBps}
              targetBps={vault.capitalStructure.targetJuniorBps}
              maintenanceBps={vault.capitalStructure.maintenanceJuniorBps}
              variant="compact"
            />
          </span>
        )}
      </span>
      {tranched ? (
        <TranchePair
          senior={vault.seniorPps != null ? vault.seniorPps.toFixed(6) : "—"}
          junior={vault.juniorPps != null ? vault.juniorPps.toFixed(6) : "—"}
        />
      ) : (
        <span>{pps != null ? pps.toFixed(6) : "—"}</span>
      )}
      {tranched ? (
        <TranchePair
          senior={seniorTvl != null ? `${formatPrice(seniorTvl, { grouping: true })} ${symbol}` : "—"}
          junior={juniorTvl != null ? `${formatPrice(juniorTvl, { grouping: true })} ${symbol}` : "—"}
        />
      ) : (
        <span>
          {tvl != null ? `${formatPrice(tvl, { grouping: true })} ${symbol}` : "—"}
        </span>
      )}
      <span>{vault.pendingWithdrawals}</span>
      <span>{(vault.curatorFeeBps / 100).toFixed(2)}%</span>
      <span>{fmtDurationMs(vault.lockupMs)}</span>
      <span>
        <Address value={vault.curator} label="Curator" />
      </span>
    </div>
  );
}

// ── create-vault form ───────────────────────────────────────────────────────

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label style={{ fontSize: 11, opacity: 0.8, display: "block" }}>
      {label}
      {children}
    </label>
  );
}

function CreateVaultCard() {
  const account = useCurrentAccount();
  const address = account?.address ?? null;
  const actions = useTradingVaultActions();
  const cfgQ = useVaultProtocolConfigId();

  const [open, setOpen] = useState(false);
  // Off mainnet/prod, Bluefin's staging margin asset is selectable even
  // though token-info doesn't serve it — a vault must be *created* with it to
  // fund a Bluefin account at all (SO-311).
  const tokens = useMemo(
    () => [
      ...SUPPORTED_TOKENS.filter((t) => t.enabled),
      ...(BLUEFIN_TEST_ENABLED ? [BLUEFIN_TEST_USDC] : []),
    ],
    [],
  );
  const [coinType, setCoinType] = useState<string>(tokens[0]?.coinType ?? "");
  const [lockupDays, setLockupDays] = useState("7");
  const [feeBps, setFeeBps] = useState("1000");
  const [unwindHours, setUnwindHours] = useState("48");
  // Advanced: senior/junior capital structure (SO-418). Immutable at
  // creation — validated on-chain against the protocol floors/caps.
  const [tranched, setTranched] = useState(false);
  const [hurdleBps, setHurdleBps] = useState("500");
  const [targetBps, setTargetBps] = useState("2000");
  const [maintenanceBps, setMaintenanceBps] = useState("1000");
  const [upsideCode, setUpsideCode] = useState<0 | 1 | 2>(0);
  const [participationBps, setParticipationBps] = useState("0");
  const [capBps, setCapBps] = useState("0");

  const cfgId = cfgQ.data ?? null;
  const lockupNum = Number(lockupDays);
  const feeNum = Number(feeBps);
  const unwindNum = Number(unwindHours);
  const hurdleNum = Number(hurdleBps);
  const targetNum = Number(targetBps);
  const maintenanceNum = Number(maintenanceBps);
  const participationNum = upsideCode === 0 ? 0 : Number(participationBps);
  const capNum = upsideCode === 1 ? Number(capBps) : 0;
  const trancheValid =
    !tranched ||
    (Number.isInteger(hurdleNum) && hurdleNum >= 0 &&
      Number.isInteger(targetNum) && targetNum > 0 &&
      Number.isInteger(maintenanceNum) && maintenanceNum > 0 &&
      maintenanceNum <= targetNum &&
      Number.isInteger(participationNum) && participationNum >= 0 && participationNum <= 10_000 &&
      Number.isInteger(capNum) && capNum >= 0 &&
      (upsideCode !== 1 || capNum > 0));
  const valid =
    !!coinType &&
    Number.isFinite(lockupNum) && lockupNum >= 0 &&
    Number.isInteger(feeNum) && feeNum >= 0 &&
    Number.isFinite(unwindNum) && unwindNum >= 0 &&
    trancheValid;

  // Why the CTA is dead, when it is — shown as helper text as well as a title,
  // since hovering isn't a thing on touch.
  const blockedReason = !address
    ? "Connect a wallet to create a vault"
    : !cfgId
      ? cfgQ.isLoading
        ? "Resolving protocol config…"
        : "Protocol config not found for this deployment"
      : tranched && !trancheValid
        ? "Tranche terms invalid — maintenance must be > 0 and ≤ target"
        : undefined;

  const onCreate = () => {
    if (!valid || !cfgId) return;
    actions.createVault({
      protocolConfigId: cfgId,
      depositCoinType: coinType,
      lockupMs: Math.round(lockupNum * 86_400_000),
      curatorFeeBps: feeNum,
      unwindGraceMs: Math.round(unwindNum * 3_600_000),
      capital: tranched
        ? {
            structureCode: 1,
            seniorHurdleBpsAnnual: hurdleNum,
            targetJuniorBps: targetNum,
            maintenanceJuniorBps: maintenanceNum,
            upsideCode,
            residualParticipationBps: participationNum,
            totalReturnCapBps: capNum,
          }
        : UNTRANCHED_CAPITAL,
      // §9.2 terms binding — served by token-info per deployment; version 1
      // with no hash on records predating SO-418.
      termsVersion: TRADING_VAULT_OBJECTS?.termsVersion ?? 1,
      specHashHex: TRADING_VAULT_OBJECTS?.specHash ?? "",
    });
  };

  return (
    <div className="vault-card" style={{ marginTop: 16 }}>
      <div className="vault-card__head" style={{ marginBottom: open ? 12 : 0 }}>
        Create vault
        <button
          className="vault-howto__trigger"
          style={{ marginLeft: "auto" }}
          onClick={() => setOpen((o) => !o)}
          aria-expanded={open}
        >
          {open ? "Hide" : "New vault"}
        </button>
      </div>
      {open && (
        <>
          <div
            style={{
              display: "grid",
              gridTemplateColumns: "repeat(auto-fit, minmax(180px, 1fr))",
              gap: 10,
            }}
          >
            <Field label="Accounting asset">
              <select style={curatorFieldStyle} value={coinType} onChange={(e) => setCoinType(e.target.value)}>
                {tokens.map((t) => (
                  <option key={t.coinType} value={t.coinType}>
                    {t.ticker} — {t.name}
                  </option>
                ))}
              </select>
            </Field>
            <Field label="Lockup (days)">
              <input style={curatorFieldStyle} type="number" min="0" value={lockupDays} onChange={(e) => setLockupDays(e.target.value)} />
            </Field>
            <Field label="Curator fee (bps)">
              <input style={curatorFieldStyle} type="number" min="0" step="1" value={feeBps} onChange={(e) => setFeeBps(e.target.value)} />
            </Field>
            <Field label="Unwind grace (hours)">
              <input style={curatorFieldStyle} type="number" min="0" value={unwindHours} onChange={(e) => setUnwindHours(e.target.value)} />
            </Field>
          </div>
          <div style={{ marginTop: 12 }}>
            <button
              className="vault-howto__trigger"
              onClick={() => setTranched((t) => !t)}
              aria-expanded={tranched}
            >
              {tranched ? "▾ Advanced: tranches (on)" : "▸ Advanced: tranches"}
            </button>
            {tranched && (
              <div style={{ marginTop: 10 }}>
                <div
                  style={{
                    display: "grid",
                    gridTemplateColumns: "repeat(auto-fit, minmax(180px, 1fr))",
                    gap: 10,
                  }}
                >
                  <Field label="Senior hurdle (bps / year)">
                    <input
                      style={curatorFieldStyle}
                      type="number"
                      min="0"
                      step="1"
                      value={hurdleBps}
                      onChange={(e) => setHurdleBps(e.target.value)}
                    />
                  </Field>
                  <Field label="Target junior buffer (bps of NAV)">
                    <input
                      style={curatorFieldStyle}
                      type="number"
                      min="0"
                      step="1"
                      value={targetBps}
                      onChange={(e) => setTargetBps(e.target.value)}
                    />
                  </Field>
                  <Field label="Maintenance junior buffer (bps of NAV)">
                    <input
                      style={curatorFieldStyle}
                      type="number"
                      min="0"
                      step="1"
                      value={maintenanceBps}
                      onChange={(e) => setMaintenanceBps(e.target.value)}
                    />
                  </Field>
                  <Field label="Senior upside">
                    <select
                      style={curatorFieldStyle}
                      value={upsideCode}
                      onChange={(e) => setUpsideCode(Number(e.target.value) as 0 | 1 | 2)}
                    >
                      <option value={0}>Preferred only — senior upside stops at its claim</option>
                      <option value={1}>Capped participating</option>
                      <option value={2}>Uncapped participating</option>
                    </select>
                  </Field>
                  {upsideCode !== 0 && (
                    <Field label="Residual participation (bps)">
                      <input
                        style={curatorFieldStyle}
                        type="number"
                        min="0"
                        max="10000"
                        step="1"
                        value={participationBps}
                        onChange={(e) => setParticipationBps(e.target.value)}
                      />
                    </Field>
                  )}
                  {upsideCode === 1 && (
                    <Field label="Total return cap (bps)">
                      <input
                        style={curatorFieldStyle}
                        type="number"
                        min="0"
                        step="1"
                        value={capBps}
                        onChange={(e) => setCapBps(e.target.value)}
                      />
                    </Field>
                  )}
                </div>
                <div className="vault-prose__muted" style={{ marginTop: 8 }}>
                  Junior absorbs first loss; senior accrues the hurdle as a
                  priority claim (not guaranteed yield). Maintenance must be
                  &gt; 0 and ≤ target; the hurdle, both buffers, and
                  participation are also validated on-chain against the
                  protocol floors/caps at creation.
                </div>
                <div className="dash-alert" style={{ marginTop: 8 }} role="note">
                  These terms are <strong>immutable at creation</strong> — the
                  capital structure can never be changed for the life of the
                  vault. Read the{" "}
                  <a
                    href="https://github.com/ewitulsk/SuiOptions/blob/staging/docs/trading-vault-v2/disclosures.md"
                    target="_blank"
                    rel="noreferrer"
                  >
                    terms &amp; risk disclosures
                  </a>{" "}
                  (terms v{TRADING_VAULT_OBJECTS?.termsVersion ?? 1}) before
                  creating a tranched vault.
                </div>
              </div>
            )}
          </div>
          <div className="vault-prose__muted" style={{ marginTop: 8 }}>
            Your connected wallet becomes the curator. The role is a transferable cap you can
            hand off later. Creation is not gas-sponsored — your wallet pays gas.
          </div>
          <button
            className="vault-invest__cta"
            style={{ marginTop: 12 }}
            disabled={!!actions.busy || !valid || !address || !cfgId}
            onClick={onCreate}
            title={blockedReason}
          >
            {actions.busy ? `${actions.busy}…` : tranched ? "Create tranched vault" : "Create vault"}
          </button>
          {blockedReason && (
            <div className="vault-card__foot vault-prose__muted">{blockedReason}</div>
          )}
        </>
      )}
      {actions.toast && <Toast message={actions.toast.message} variant={actions.toast.variant} />}
    </div>
  );
}
