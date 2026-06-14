// Covered-call vault page (SO vault system — PRs 137/148/168).
//
// Reads the api-service vault endpoints (`/vaults`, `/vaults/:id/rounds`) and
// the user's on-chain receipts/share balance, and drives every invest action
// (deposit / claim / initiate+complete withdraw / cancel) through
// `useVaultActions`, which routes to the wallet or session-login custody path.
//
// Several "informed-decision" fields have NO endpoint yet (fees, strike band,
// round cadence, live phase). They are rendered from MOCK constants, each
// flagged `MOCK:` inline — see the "endpoints to build" note in the PR body.

import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useSuiClient } from "@mysten/dapp-kit";

import type { Vault, VaultRound } from "../api/vaults";
import { useVault, useVaultRounds, useVaultApyHistory, useOwnedVaultReceipts, useShareBalance, useVaults } from "../api/useVaults";
import { useVaultActions } from "../state/vault";
import { useUserIdentity } from "../session/identity";
import { readCustodyObjectIds } from "../session/accounts";
import { VaultApyChart } from "../components/VaultApyChart";
import { Toast } from "../components/Toast";
import { formatPrice } from "../format";

// ─────────────────────── MOCK config (no endpoint yet) ───────────────────────
// These come from the on-chain `VaultConfig` (contracts/sources/vault.move),
// which the api-service does NOT serve. Hard-coded here so the page reads as a
// real product; replace with a `GET /vaults/:id/config` payload when it ships.
const MOCK_CONFIG = {
  mgmtFeePct: 2.0, // MOCK: VaultConfig.mgmt_fee_bps_annual (annualized management fee)
  perfFeePct: 10.0, // MOCK: VaultConfig.perf_fee_bps (performance fee on premium)
  minStrikeOverSpotPct: 3, // MOCK: VaultConfig.min_strike_bps_over_spot
  maxStrikeOverSpotPct: 60, // MOCK: VaultConfig.max_strike_bps_over_spot
  roundCadence: "Weekly", // MOCK: derived from VaultConfig.round_ms
  capacityUnderlying: null as number | null, // MOCK: vaults have no hard cap on-chain; null = uncapped
};

function scaled(raw: string | null | undefined, decimals: number | null): number | null {
  if (raw == null || decimals == null) return null;
  return Number(raw) / 10 ** decimals;
}

function toRaw(amount: number, decimals: number): bigint {
  // Round to the nearest atomic unit. Fine for the amounts a deposit UI takes;
  // BigInt keeps the on-chain value exact once scaled.
  return BigInt(Math.round(amount * 10 ** decimals));
}

function fmtPct(x: number | null | undefined, digits = 2): string {
  if (x == null || !Number.isFinite(x)) return "—";
  return `${(x * 100).toFixed(digits)}%`;
}

function fmtDate(ms: number | null | undefined): string {
  if (ms == null) return "—";
  return new Date(ms).toLocaleDateString(undefined, { month: "short", day: "numeric", year: "numeric" });
}

export function VaultScreen() {
  const vaults = useVaults();
  const [selected, setSelected] = useState<string | null>(null);

  // Default to the first vault once the list loads.
  useEffect(() => {
    if (!selected && vaults.data && vaults.data.length > 0) {
      setSelected(vaults.data[0].vault_id);
    }
  }, [vaults.data, selected]);

  return (
    <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
      <div className="app__wrap">
        <div className="dash-hero">
          <div className="dash-hero__eyebrow">automated strategy</div>
          <h1 className="dash-hero__title">Vaults</h1>
          <div className="dash-hero__addr">
            Deposit once; the vault writes covered calls every round and compounds the premium.
          </div>
        </div>

        {vaults.isLoading && <div className="vault-note">Loading vaults…</div>}
        {vaults.isError && (
          <div className="dash-alert" role="alert">
            Couldn't load vaults: {vaults.error.message}
          </div>
        )}
        {vaults.data && vaults.data.length === 0 && (
          <div className="dash-empty">
            <div className="dash-empty__title">no vaults yet.</div>
            <div className="dash-empty__sub">
              No covered-call vault has been deployed on this network.
            </div>
          </div>
        )}

        {vaults.data && vaults.data.length > 1 && (
          <div className="vault-picker">
            {vaults.data.map((v) => (
              <button
                key={v.vault_id}
                className={"vault-picker__item" + (v.vault_id === selected ? " is-active" : "")}
                onClick={() => setSelected(v.vault_id)}
              >
                {v.underlying_symbol} covered call
              </button>
            ))}
          </div>
        )}

        {selected && <VaultDetail vaultId={selected} />}
      </div>
    </div>
  );
}

function VaultDetail({ vaultId }: { vaultId: string }) {
  const vaultQ = useVault(vaultId);
  const roundsQ = useVaultRounds(vaultId);
  const apyQ = useVaultApyHistory(vaultId);

  if (vaultQ.isLoading || !vaultQ.data) {
    return <div className="vault-note">Loading vault…</div>;
  }
  const vault = vaultQ.data;
  const rounds = roundsQ.data ?? [];

  return (
    <>
      <VaultStats vault={vault} rounds={rounds} />
      <div className="vault-grid">
        <div className="vault-grid__main">
          <VaultApyChart points={apyQ.data ?? []} loading={apyQ.isLoading} />
          <StrategyCard vault={vault} />
          <CurrentRoundCard vault={vault} rounds={rounds} />
          <TrackRecord vault={vault} rounds={rounds} />
        </div>
        <div className="vault-grid__side">
          <InvestPanel vault={vault} />
          <ParamsCard vault={vault} />
        </div>
      </div>
    </>
  );
}

function VaultStats({ vault, rounds }: { vault: Vault; rounds: VaultRound[] }) {
  const sharePositionValue = useMyPositionValue(vault);
  const finalized = rounds.filter((r) => r.pps != null).length;
  return (
    <div className="vault-stats">
      <div className="vault-stat vault-stat--hero">
        <div className="vault-stat__label">Net APY</div>
        <div className="vault-stat__val is-pos">{vault.apy != null ? fmtPct(vault.apy) : "—"}</div>
        <div className="vault-stat__sub">
          {finalized >= 2 ? "annualized, last round" : "needs 2 finalized rounds"}
        </div>
      </div>
      <div className="vault-stat">
        <div className="vault-stat__label">TVL</div>
        <div className="vault-stat__val">
          {vault.tvl != null ? formatPrice(vault.tvl, { grouping: true }) : "—"}
          <span className="vault-stat__unit"> {vault.underlying_symbol}</span>
        </div>
        <div className="vault-stat__sub">total value locked</div>
      </div>
      <div className="vault-stat">
        <div className="vault-stat__label">Price / share</div>
        <div className="vault-stat__val">
          {vault.pps != null ? vault.pps.toFixed(6) : "1.000000"}
          <span className="vault-stat__unit"> {vault.underlying_symbol}</span>
        </div>
        <div className="vault-stat__sub">round {vault.round}</div>
      </div>
      <div className="vault-stat">
        <div className="vault-stat__label">Your position</div>
        <div className="vault-stat__val">
          {sharePositionValue != null ? formatPrice(sharePositionValue, { grouping: true }) : "—"}
          <span className="vault-stat__unit"> {vault.underlying_symbol}</span>
        </div>
        <div className="vault-stat__sub">value of held shares</div>
      </div>
    </div>
  );
}

function StrategyCard({ vault }: { vault: Vault }) {
  return (
    <div className="vault-card">
      <div className="vault-card__head">
        <span className="panel__head-dot" />
        How this vault works
      </div>
      <div className="vault-card__body vault-prose">
        <p>
          Each round, the vault writes covered calls on its {vault.underlying_symbol} against{" "}
          {vault.settlement_symbol}. It sells the calls to market makers via on-chain RFQ auctions
          and collects the premium up front.
        </p>
        <ol>
          <li><b>Deposit</b> {vault.underlying_symbol} — it's queued for the next round (never exposed to the round already running).</li>
          <li>When the round finalizes, your deposit mints <b>shares</b> at that round's price-per-share. Claim them anytime.</li>
          <li>The vault's price-per-share rises by the net premium each successful round; that's your yield.</li>
          <li>To exit, <b>initiate a withdrawal</b> (escrows shares for the current round's P&L), then <b>complete</b> it once the round finalizes.</li>
        </ol>
        <p className="vault-prose__muted">
          Covered calls cap upside above the strike: in a sharp rally the vault may be assigned and
          forgo gains beyond the strike, keeping the premium. Principal is exposed to{" "}
          {vault.underlying_symbol} price like any spot holding.
        </p>
      </div>
    </div>
  );
}

function CurrentRoundCard({ vault, rounds }: { vault: Vault; rounds: VaultRound[] }) {
  const current = rounds.find((r) => r.round === vault.round);
  // MOCK: the api-service exposes no live phase flag. Heuristic only — a
  // selected bucket means the round is actively selling/holding calls.
  const phase = vault.current_bucket ? "Active — selling / holding" : "Settling — between rounds";
  return (
    <div className="vault-card">
      <div className="vault-card__head">
        <span className="panel__head-dot" />
        Current round · #{vault.round}
      </div>
      <div className="vault-kv">
        <div className="vault-kv__row">
          <span>Status</span>
          <span>{phase} <em className="vault-mock">mock</em></span>
        </div>
        <div className="vault-kv__row">
          <span>Strike</span>
          <span>
            {current?.strike != null ? `$${formatPrice(current.strike, { grouping: true })}` : "—"}
          </span>
        </div>
        <div className="vault-kv__row">
          <span>Expiry</span>
          <span>{fmtDate(current?.expiry_ms ?? null)}</span>
        </div>
        <div className="vault-kv__row">
          <span>Deposits</span>
          <span>{vault.deposits_paused ? "Paused" : "Open"}</span>
        </div>
      </div>
    </div>
  );
}

function TrackRecord({ vault, rounds }: { vault: Vault; rounds: VaultRound[] }) {
  const finalized = rounds.filter((r) => r.pps != null).sort((a, b) => b.round - a.round);
  const sDec = vault.settlement_decimals;
  return (
    <div className="vault-card">
      <div className="vault-card__head">
        <span className="panel__head-dot" />
        Track record
      </div>
      {finalized.length === 0 ? (
        <div className="vault-card__body vault-prose__muted">
          No finalized rounds yet — the first round's results will appear here.
        </div>
      ) : (
        <div className="vault-table">
          <div className="vault-table__head">
            <span>Round</span>
            <span>Strike</span>
            <span>Expiry</span>
            <span>PPS</span>
            <span>Premium</span>
          </div>
          {finalized.map((r) => {
            const premium = scaled(r.premium_collected_raw, sDec);
            return (
              <div className="vault-table__row" key={r.round}>
                <span>#{r.round}</span>
                <span>{r.strike != null ? `$${formatPrice(r.strike)}` : "—"}</span>
                <span>{fmtDate(r.expiry_ms)}</span>
                <span>{r.pps != null ? r.pps.toFixed(6) : "—"}</span>
                <span className="is-pos">
                  {premium != null ? `+${formatPrice(premium)} ${vault.settlement_symbol}` : "—"}
                </span>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

function ParamsCard({ vault }: { vault: Vault }) {
  return (
    <div className="vault-card">
      <div className="vault-card__head">
        <span className="panel__head-dot" />
        Parameters & fees <em className="vault-mock">mock</em>
      </div>
      <div className="vault-kv">
        <div className="vault-kv__row">
          <span>Management fee</span>
          <span>{MOCK_CONFIG.mgmtFeePct.toFixed(1)}% / yr</span>
        </div>
        <div className="vault-kv__row">
          <span>Performance fee</span>
          <span>{MOCK_CONFIG.perfFeePct.toFixed(0)}% of premium</span>
        </div>
        <div className="vault-kv__row">
          <span>Strike band</span>
          <span>+{MOCK_CONFIG.minStrikeOverSpotPct}% to +{MOCK_CONFIG.maxStrikeOverSpotPct}% over spot</span>
        </div>
        <div className="vault-kv__row">
          <span>Round cadence</span>
          <span>{MOCK_CONFIG.roundCadence}</span>
        </div>
        <div className="vault-kv__row">
          <span>Capacity</span>
          <span>{MOCK_CONFIG.capacityUnderlying == null ? "Uncapped" : `${MOCK_CONFIG.capacityUnderlying} ${vault.underlying_symbol}`}</span>
        </div>
      </div>
      <div className="vault-card__foot vault-prose__muted">
        These come from the on-chain <code>VaultConfig</code>, which has no endpoint yet — values
        shown are placeholders.
      </div>
    </div>
  );
}

// Position value (held shares × pps), in display underlying units. Shares are
// denominated in the underlying's decimals (1 share == 1 underlying at pps 1.0).
function useMyPositionValue(vault: Vault): number | null {
  const identity = useUserIdentity();
  const session = identity?.kind === "session" ? identity.session : null;
  const balQ = useShareBalance(
    identity?.address ?? null,
    vault.share_type,
    session ? session.balances : undefined,
  );
  if (balQ.data == null || vault.underlying_decimals == null || vault.pps == null) return null;
  const shares = Number(balQ.data) / 10 ** vault.underlying_decimals;
  return shares * vault.pps;
}

function InvestPanel({ vault }: { vault: Vault }) {
  const identity = useUserIdentity();
  const session = identity?.kind === "session" ? identity.session : null;
  const address = identity?.address ?? null;
  const client = useSuiClient();
  const actions = useVaultActions();
  const [tab, setTab] = useState<"deposit" | "withdraw">("deposit");
  const [amount, setAmount] = useState("");

  // Session receipts/share coins live in custody; read the on-account object
  // index so claim/withdraw can find their receipt object ids.
  const custodyQ = useQuery({
    queryKey: ["custody-objs", session?.optionsAccountId],
    enabled: !!session?.optionsAccountId,
    refetchInterval: 10_000,
    queryFn: () => readCustodyObjectIds(client, session!.optionsAccountId as string),
  });

  const receiptsQ = useOwnedVaultReceipts(
    address,
    vault.vault_id,
    session ? (custodyQ.data ?? []) : undefined,
  );
  const shareBalQ = useShareBalance(
    address,
    vault.share_type,
    session ? session.balances : undefined,
  );

  const uDec = vault.underlying_decimals;
  const amountNum = Number(amount) || 0;
  const shareDisplay = uDec != null && shareBalQ.data != null ? Number(shareBalQ.data) / 10 ** uDec : 0;

  if (!address) {
    return (
      <div className="vault-card vault-invest">
        <div className="vault-card__head"><span className="panel__head-dot" />Invest</div>
        <div className="vault-card__body vault-prose__muted">
          Connect a wallet or sign in to deposit into this vault.
        </div>
      </div>
    );
  }

  const onSubmit = () => {
    if (uDec == null) return;
    if (tab === "deposit") {
      if (amountNum <= 0) return;
      actions.deposit(vault, toRaw(amountNum, uDec));
    } else {
      if (amountNum <= 0) return;
      actions.initiateWithdraw(vault, toRaw(amountNum, uDec));
    }
    setAmount("");
  };

  const deposits = receiptsQ.data?.deposits ?? [];
  const withdraws = receiptsQ.data?.withdraws ?? [];

  return (
    <div className="vault-card vault-invest">
      <div className="vault-invest__tabs">
        <button className={"vault-invest__tab" + (tab === "deposit" ? " is-active" : "")} onClick={() => setTab("deposit")}>Deposit</button>
        <button className={"vault-invest__tab" + (tab === "withdraw" ? " is-active" : "")} onClick={() => setTab("withdraw")}>Withdraw</button>
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
        <span className="vault-invest__unit">
          {tab === "deposit" ? vault.underlying_symbol : "shares"}
        </span>
      </div>
      <div className="vault-invest__bal">
        {tab === "withdraw"
          ? `${shareDisplay.toFixed(4)} shares available`
          : vault.deposits_paused
            ? "deposits are paused"
            : "deposited funds enter next round"}
      </div>

      <button
        className="vault-invest__cta"
        disabled={!!actions.busy || amountNum <= 0 || (tab === "deposit" && vault.deposits_paused) || uDec == null}
        onClick={onSubmit}
      >
        {actions.busy
          ? `${actions.busy}…`
          : tab === "deposit"
            ? `Deposit ${vault.underlying_symbol}`
            : "Initiate withdrawal"}
      </button>

      {/* Outstanding receipts: claim shares / complete withdrawal / cancel. */}
      {(deposits.length > 0 || withdraws.length > 0) && (
        <div className="vault-invest__receipts">
          <div className="vault-invest__receipts-head">Your receipts</div>
          {deposits.map((r) => {
            const claimable = vault.round >= r.round; // pps[round-1] exists
            const cancellable = r.round > vault.round; // round hasn't started
            const amt = scaled(r.amount_raw, uDec);
            return (
              <div className="vault-receipt" key={r.object_id}>
                <span>Deposit · round {r.round} · {amt != null ? amt.toFixed(4) : "?"} {vault.underlying_symbol}</span>
                {claimable ? (
                  <button disabled={!!actions.busy} onClick={() => actions.claim(vault, r.object_id)}>Claim shares</button>
                ) : cancellable ? (
                  <button disabled={!!actions.busy} onClick={() => actions.cancelDeposit(vault, r.object_id)}>Cancel</button>
                ) : (
                  <span className="vault-receipt__pending">pending</span>
                )}
              </div>
            );
          })}
          {withdraws.map((r) => {
            const payable = vault.round > r.round; // round r finalized
            const amt = scaled(r.amount_raw, uDec);
            return (
              <div className="vault-receipt" key={r.object_id}>
                <span>Withdraw · round {r.round} · {amt != null ? amt.toFixed(4) : "?"} shares</span>
                {payable ? (
                  <button disabled={!!actions.busy} onClick={() => actions.completeWithdraw(vault, r.object_id)}>Complete</button>
                ) : (
                  <span className="vault-receipt__pending">finalizing</span>
                )}
              </div>
            );
          })}
        </div>
      )}

      {actions.toast && <Toast message={actions.toast.message} variant={actions.toast.variant} />}
    </div>
  );
}
