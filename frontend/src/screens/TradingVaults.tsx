// Curated trading-vault list (SO-288).
//
// Reads the api-service `/trading-vaults` endpoint and renders every vault as
// a row linking into its detail (`/vaults/:vaultId`), plus a collapsible
// "Create vault" form driving `vault::create_vault<T>` through the shared
// submit path. Renders an "unavailable" empty state on networks with no
// trading-vault deployment.

import { useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useCurrentAccount } from "@mysten/dapp-kit";

import { tradingVaultPps, tradingVaultTvl, tokenForCoinType, type TradingVault } from "../api/tradingVaults";
import { useTradingVaults, useVaultProtocolConfigId } from "../api/useTradingVaults";
import { useTradingVaultActions } from "../state/tradingVault";
import { SUPPORTED_TOKENS, TRADING_VAULT_PACKAGE_ID } from "../config";
import { TokenLogo } from "../components/TokenLogo";
import { Toast } from "../components/Toast";
import { formatPrice } from "../format";

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

export function TradingVaults() {
  const vaultsQ = useTradingVaults();
  const navigate = useNavigate();

  if (!TRADING_VAULT_PACKAGE_ID) {
    return (
      <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
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

  return (
    <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
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
            <div className="vault-card__head">All vaults</div>
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
              {vaults.map((v) => (
                <TradingVaultRow
                  key={v.vaultId}
                  vault={v}
                  onOpen={() => navigate(`/vaults/${v.vaultId}`)}
                />
              ))}
            </div>
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
  return (
    <div
      className="vault-table__row"
      style={{ ...GRID, cursor: "pointer", alignItems: "center" }}
      onClick={onOpen}
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
      <span title={vault.curator}>{shortHex(vault.curator)}</span>
    </div>
  );
}

// ── create-vault form ───────────────────────────────────────────────────────

const fieldStyle: React.CSSProperties = {
  width: "100%",
  padding: 6,
  borderRadius: 6,
  border: "1px solid var(--aqua-line, rgba(92,107,122,0.25))",
  background: "transparent",
  color: "inherit",
};

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
  const tokens = useMemo(() => SUPPORTED_TOKENS.filter((t) => t.enabled), []);
  const [coinType, setCoinType] = useState<string>(tokens[0]?.coinType ?? "");
  // Empty means "default to the connected wallet at submit time".
  const [curator, setCurator] = useState("");
  const [lockupDays, setLockupDays] = useState("7");
  const [feeBps, setFeeBps] = useState("1000");
  const [rotation, setRotation] = useState("0");
  const [maxPositions, setMaxPositions] = useState("16");
  const [unwindHours, setUnwindHours] = useState("48");

  const cfgId = cfgQ.data ?? null;
  const curatorAddr = curator.trim() || address || "";
  const lockupNum = Number(lockupDays);
  const feeNum = Number(feeBps);
  const maxPosNum = Number(maxPositions);
  const unwindNum = Number(unwindHours);
  const valid =
    !!coinType &&
    !!curatorAddr &&
    Number.isFinite(lockupNum) && lockupNum >= 0 &&
    Number.isInteger(feeNum) && feeNum >= 0 &&
    Number.isInteger(maxPosNum) && maxPosNum > 0 &&
    Number.isFinite(unwindNum) && unwindNum >= 0;

  const onCreate = () => {
    if (!valid || !cfgId) return;
    actions.createVault({
      protocolConfigId: cfgId,
      depositCoinType: coinType,
      curator: curatorAddr,
      lockupMs: Math.round(lockupNum * 86_400_000),
      curatorFeeBps: feeNum,
      rotationAuthority: Number(rotation),
      maxPositions: maxPosNum,
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
              <select style={fieldStyle} value={coinType} onChange={(e) => setCoinType(e.target.value)}>
                {tokens.map((t) => (
                  <option key={t.coinType} value={t.coinType}>
                    {t.ticker} — {t.name}
                  </option>
                ))}
              </select>
            </Field>
            <Field label="Curator address">
              <input
                style={fieldStyle}
                value={curator}
                onChange={(e) => setCurator(e.target.value)}
                placeholder={address ?? "0x…"}
              />
            </Field>
            <Field label="Lockup (days)">
              <input style={fieldStyle} type="number" min="0" value={lockupDays} onChange={(e) => setLockupDays(e.target.value)} />
            </Field>
            <Field label="Curator fee (bps)">
              <input style={fieldStyle} type="number" min="0" step="1" value={feeBps} onChange={(e) => setFeeBps(e.target.value)} />
            </Field>
            <Field label="Rotation authority">
              <select style={fieldStyle} value={rotation} onChange={(e) => setRotation(e.target.value)}>
                <option value="0">Creator</option>
                <option value="1">Curator</option>
                <option value="2">Either</option>
              </select>
            </Field>
            <Field label="Max positions">
              <input style={fieldStyle} type="number" min="1" step="1" value={maxPositions} onChange={(e) => setMaxPositions(e.target.value)} />
            </Field>
            <Field label="Unwind grace (hours)">
              <input style={fieldStyle} type="number" min="0" value={unwindHours} onChange={(e) => setUnwindHours(e.target.value)} />
            </Field>
          </div>
          <button
            className="vault-invest__cta"
            style={{ marginTop: 12 }}
            disabled={!!actions.busy || !valid || !address || !cfgId}
            onClick={onCreate}
            title={
              !address
                ? "Connect a wallet to create a vault"
                : !cfgId
                  ? cfgQ.isLoading
                    ? "Resolving protocol config…"
                    : "Protocol config not found for this deployment"
                  : undefined
            }
          >
            {actions.busy ? `${actions.busy}…` : "Create vault"}
          </button>
        </>
      )}
      {actions.toast && <Toast message={actions.toast.message} variant={actions.toast.variant} />}
    </div>
  );
}
