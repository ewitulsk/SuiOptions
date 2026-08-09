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

import { tradingVaultPps, tradingVaultTvl, tokenForCoinType, type TradingVault } from "../api/tradingVaults";
import { useTradingVaults, useVaultProtocolConfigId } from "../api/useTradingVaults";
import { useTradingVaultActions } from "../state/tradingVault";
import { BLUEFIN_TEST_ENABLED, BLUEFIN_TEST_USDC } from "../bluefinTest";
import { SUPPORTED_TOKENS, TRADING_VAULT_PACKAGE_ID } from "../config";
import { Address } from "../components/Address";
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

function TradingVaultRow({ vault, onOpen }: { vault: TradingVault; onOpen: () => void }) {
  const token = tokenForCoinType(vault.depositAsset);
  const symbol = token?.ticker ?? shortHex(vault.depositAsset.split("::")[0] ?? vault.depositAsset);
  const pps = tradingVaultPps(vault);
  const tvl = tradingVaultTvl(vault, token?.decimals ?? null);
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
        <StateBadge state={vault.state} />
      </span>
      <span>{pps != null ? pps.toFixed(6) : "—"}</span>
      <span>
        {tvl != null ? `${formatPrice(tvl, { grouping: true })} ${symbol}` : "—"}
      </span>
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

  const cfgId = cfgQ.data ?? null;
  const lockupNum = Number(lockupDays);
  const feeNum = Number(feeBps);
  const unwindNum = Number(unwindHours);
  const valid =
    !!coinType &&
    Number.isFinite(lockupNum) && lockupNum >= 0 &&
    Number.isInteger(feeNum) && feeNum >= 0 &&
    Number.isFinite(unwindNum) && unwindNum >= 0;

  // Why the CTA is dead, when it is — shown as helper text as well as a title,
  // since hovering isn't a thing on touch.
  const blockedReason = !address
    ? "Connect a wallet to create a vault"
    : !cfgId
      ? cfgQ.isLoading
        ? "Resolving protocol config…"
        : "Protocol config not found for this deployment"
      : undefined;

  const onCreate = () => {
    if (!valid || !cfgId) return;
    actions.createVault({
      protocolConfigId: cfgId,
      depositCoinType: coinType,
      lockupMs: Math.round(lockupNum * 86_400_000),
      curatorFeeBps: feeNum,
      unwindGraceMs: Math.round(unwindNum * 3_600_000),
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
            <Field label="Deposit asset">
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
          <div className="vault-prose__muted" style={{ marginTop: 8 }}>
            Your connected wallet becomes the curator. The role is a transferable cap you can
            hand off later.
          </div>
          <button
            className="vault-invest__cta"
            style={{ marginTop: 12 }}
            disabled={!!actions.busy || !valid || !address || !cfgId}
            onClick={onCreate}
            title={blockedReason}
          >
            {actions.busy ? `${actions.busy}…` : "Create vault"}
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
