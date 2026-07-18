// Curated trading-vault detail (SO-288), route `/vaults/:vaultId`.
//
// Header terms + share price/TVL from the api-service detail endpoint, the
// custodied-positions table, and the user panel: deposit (begin_appraisal →
// deposit, only while the vault holds nothing but its deposit asset) and
// request-withdraw (always available; shares are a u128).

import { useState } from "react";
import { Link, useParams } from "react-router-dom";
import { useCurrentAccount } from "@mysten/dapp-kit";

import {
  tokenForCoinType,
  tradingVaultPps,
  tradingVaultTvl,
  type TradingVaultDetail as TradingVaultDetailDto,
  type TradingVaultPosition,
} from "../api/tradingVaults";
import { useTradingVault, useVaultProtocolConfigId } from "../api/useTradingVaults";
import { useTradingVaultActions } from "../state/tradingVault";
import { useCoinBalance } from "../api/useCoinBalance";
import { TRADING_VAULT_PACKAGE_ID } from "../config";
import { TokenLogo } from "../components/TokenLogo";
import { Toast } from "../components/Toast";
import { formatPrice } from "../format";
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

/** Adapter short-name: the module path after the package address. */
function adapterName(adapter: string): string {
  const parts = adapter.split("::");
  return parts.length > 1 ? parts.slice(1).join("::") : shortHex(adapter);
}

export function TradingVaultDetailScreen() {
  const { vaultId } = useParams<{ vaultId: string }>();
  const vaultQ = useTradingVault(vaultId ?? null);

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

  return (
    <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
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
  const token = tokenForCoinType(vault.depositAsset);
  const symbol = token?.ticker ?? shortHex(vault.depositAsset);
  const pps = tradingVaultPps(vault);
  const tvl = tradingVaultTvl(vault, token?.decimals ?? null);

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
          <PositionsCard positions={vault.positions} />
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
          <span title={vault.curator}>{shortHex(vault.curator)}</span>
        </div>
        <div className="vault-kv__row">
          <span>Creator</span>
          <span title={vault.creator}>{shortHex(vault.creator)}</span>
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

function PositionsCard({ positions }: { positions: TradingVaultPosition[] }) {
  return (
    <div className="vault-card">
      <div className="vault-card__head">Positions · {positions.length}</div>
      {positions.length === 0 ? (
        <div className="vault-card__body vault-prose__muted">
          The vault holds only its deposit asset — no positions are custodied.
        </div>
      ) : (
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
                <span className={p.active ? "is-pos" : undefined}>
                  {p.active ? "active" : "closed"}
                </span>
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

  const [tab, setTab] = useState<"deposit" | "withdraw">("deposit");
  const [amount, setAmount] = useState("");

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

  // Deposits require a complete appraisal; with positions (or non-deposit
  // balances) in the vault, the single-leg PTB this screen builds would abort.
  const appraisalBlocked = vault.positionCount > 0;
  const depositDisabled =
    tab === "deposit" &&
    (appraisalBlocked || vault.depositsPaused || vault.state !== "open" || !cfgId);
  const depositTitle = appraisalBlocked
    ? "appraisal legs required — coming soon"
    : vault.depositsPaused
      ? "Deposits are paused"
      : vault.state !== "open"
        ? "The vault is no longer open for deposits"
        : !cfgId
          ? cfgQ.isLoading
            ? "Resolving protocol config…"
            : "Protocol config not found for this deployment"
          : undefined;

  const onSubmit = () => {
    if (decimals == null || amountNum <= 0) return;
    const raw = BigInt(Math.round(amountNum * 10 ** decimals));
    if (tab === "deposit") {
      if (!cfgId) return;
      actions.deposit({
        vaultId: vault.vaultId,
        protocolConfigId: cfgId,
        depositCoinType: vault.depositAsset,
        amountRaw: raw,
      });
    } else {
      actions.requestWithdraw({ vaultId: vault.vaultId, sharesRaw: raw });
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
          className={"vault-invest__tab" + (tab === "withdraw" ? " is-active" : "")}
          onClick={() => setTab("withdraw")}
        >
          Withdraw
        </button>
      </div>

      <div className="vault-invest__field">
        <input
          className="amount__input"
          type="number"
          min="0"
          placeholder="0.0"
          value={amount}
          onChange={(e) => setAmount(e.target.value)}
        />
        <span className="vault-invest__unit">{tab === "deposit" ? symbol : "shares"}</span>
      </div>
      <div className="vault-invest__bal">
        {tab === "deposit"
          ? balance != null
            ? `${formatPrice(balance, { grouping: true })} ${symbol} in wallet`
            : "wallet balance unavailable"
          : "queued FIFO — paid out as the curator frees funds"}
      </div>

      <button
        className="vault-invest__cta"
        disabled={!!actions.busy || amountNum <= 0 || decimals == null || depositDisabled}
        onClick={onSubmit}
        title={tab === "deposit" ? depositTitle : undefined}
      >
        {actions.busy
          ? `${actions.busy}…`
          : tab === "deposit"
            ? appraisalBlocked
              ? "Deposits need appraisal legs"
              : `Deposit ${symbol}`
            : "Request withdrawal"}
      </button>

      {tab === "deposit" && appraisalBlocked && (
        <div className="vault-card__foot vault-prose__muted">
          This vault custodies positions, so deposits need per-position appraisal
          legs — coming soon. Withdrawal requests still work.
        </div>
      )}

      {actions.toast && <Toast message={actions.toast.message} variant={actions.toast.variant} />}
    </div>
  );
}
