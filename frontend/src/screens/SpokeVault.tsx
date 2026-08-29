// Multichain spoke vault deposit/withdraw screen (docs/multichain-vault-plan.md
// §4–§5). The spoke is dumb: deposits escrow as `pending` until the hub ACK
// flips them `active` (reclaimable after DEPOSIT_TIMEOUT if no ACK lands);
// withdrawals are share-denominated requests the hub prices, pays or queues
// FIFO. Everything here is read from the spoke contract via viem over the
// configured RPC on an ~8s poll (evm/useSpokeVault.ts) — the share numbers
// shown are the contract's NON-authoritative mirror; the hub ledger rules.
//
// The EVM wallet (MetaMask-style injected) is separate from the header's Sui
// wallet, mirroring how Bridge.tsx handles Phantom.

import { useState } from "react";
import { formatUnits, parseUnits } from "viem";

import { ENV, SPOKE_CONFIG, spokeDeployed } from "../config";
import { DEPOSIT_STATUS } from "../evm/abi";
import { useEvmAccount } from "../evm/useEvmAccount";
import {
  useSpokeVault,
  type SpokeDepositRow,
  type SpokeVaultSnapshot,
} from "../evm/useSpokeVault";
import {
  approveUsdg,
  depositUsdg,
  mintTusdg,
  processPayoutQueue,
  reclaimDeposit,
  requestWithdraw,
} from "../evm/tx";
import { Toast, type ToastState } from "../components/Toast";
import { posthog } from "../lib/posthog";

// Tranche wire codes (the hub owns policy; the spoke only range-checks).
const TRANCHE_LABEL = ["Untranched", "Senior", "Junior"] as const;

const DEPOSIT_STATUS_LABEL: Record<number, string> = {
  [DEPOSIT_STATUS.None]: "unknown",
  [DEPOSIT_STATUS.Pending]: "pending", // escrowed, awaiting hub ACK
  [DEPOSIT_STATUS.Acked]: "active", // hub-ACK'd, part of vault NAV
  [DEPOSIT_STATUS.Refunded]: "refunded", // hub rejected
  [DEPOSIT_STATUS.Reclaimed]: "reclaimed",
};

/** Fee pot below this can strand outbound messages — heuristic warn level
 * until the vault-messenger alerting (plan §8) serves a real threshold. */
const FEE_POT_LOW_WEI = 5_000_000_000_000_000n; // 0.005 ETH

/** Fixed testnet faucet clip: 1,000 TUSDG per click. */
const FAUCET_UNITS = "1000";

function toRawUsdg(amount: string, decimals: number): bigint {
  try {
    const raw = parseUnits(amount.trim(), decimals);
    return raw > 0n ? raw : 0n;
  } catch {
    return 0n;
  }
}

function fmtUsdg(raw: bigint, decimals: number): string {
  return Number(formatUnits(raw, decimals)).toLocaleString("en-US", {
    maximumFractionDigits: decimals,
  });
}

function fmtEth(wei: bigint): string {
  return Number(formatUnits(wei, 18)).toLocaleString("en-US", {
    maximumFractionDigits: 5,
  });
}

/** Hub shares carry no on-spoke decimals (opaque hub units) — show raw. */
function fmtShares(shares: bigint): string {
  return shares.toLocaleString("en-US");
}

function fmtAgo(secs: number): string {
  if (secs < 0) return "just now";
  if (secs < 90) return `${secs}s ago`;
  const m = Math.floor(secs / 60);
  if (m < 90) return `${m}m ago`;
  return `${Math.floor(m / 60)}h ${m % 60}m ago`;
}

export function SpokeVault() {
  const cfg = SPOKE_CONFIG;
  const deployed = cfg !== undefined && spokeDeployed(cfg);
  const account = useEvmAccount();
  // Reads go through our own RPC, so the wallet's current chain is irrelevant
  // here — only writes force a switch (evm/tx.ts).
  const snapshot = useSpokeVault(account.address);

  const [toast, setToast] = useState<ToastState | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const [depositAmount, setDepositAmount] = useState("");
  const [depositTranche, setDepositTranche] = useState(0);

  const [withdrawTranche, setWithdrawTranche] = useState(0);
  const [withdrawShares, setWithdrawShares] = useState("");
  const [withdrawAll, setWithdrawAll] = useState(false);

  const flash = (message: string, variant: ToastState["variant"] = "success") => {
    setToast({ message, variant });
    setTimeout(() => setToast(null), 6000);
  };

  // Shared submit wrapper — Faucet.tsx's run() idiom, posthog included.
  const run = async (key: string, event: string, ok: string, fn: () => Promise<unknown>) => {
    setBusy(key);
    try {
      await fn();
      posthog.capture(event, { wallet_address: account.address });
      flash(`✓ ${ok}`);
      void snapshot.refetch();
    } catch (err) {
      posthog.captureException(err, { action: event, wallet_address: account.address });
      flash(err instanceof Error ? err.message : String(err), "error");
    } finally {
      setBusy(null);
    }
  };

  if (!cfg) {
    return (
      <div className="app__wrap">
        <Hero sub="multichain deposits into the trading vault" />
        <div className="admin-empty">
          No spoke deployment on this network; current environment is <code>{ENV}</code>.
        </div>
      </div>
    );
  }

  const s = snapshot.data;
  const user = s?.user ?? null;

  const depositRaw = toRawUsdg(depositAmount, cfg.usdgDecimals);
  const needsApproval = user !== null && depositRaw > 0n && user.allowanceRaw < depositRaw;
  const canDeposit =
    account.address !== null &&
    user !== null &&
    depositRaw > 0n &&
    depositRaw <= user.usdgBalanceRaw &&
    !(s?.paused ?? false);

  const withdrawRow = user?.tranches.find((t) => t.tranche === withdrawTranche) ?? null;
  const withdrawSharesRaw = (() => {
    try {
      const v = BigInt(withdrawShares.trim() || "0");
      return v > 0n ? v : 0n;
    } catch {
      return 0n;
    }
  })();
  const canWithdraw =
    account.address !== null &&
    withdrawRow !== null &&
    withdrawRow.inFlight === null &&
    (withdrawAll || withdrawSharesRaw > 0n);

  const connectBtn = !account.address && (
    <button
      className="bridge-form__secondary"
      onClick={() =>
        account.connect().catch((err) => flash(err instanceof Error ? err.message : String(err), "error"))
      }
      disabled={!account.hasProvider}
    >
      {account.hasProvider ? "Connect EVM wallet" : "No EVM wallet found"}
    </button>
  );

  return (
    <div className="app__wrap">
      <Hero sub={`${cfg.chainName} → Sui hub · USDG`} />

      {!deployed && (
        <div className="dash-alert">
          Spoke contracts are not deployed yet — the config carries 0x0 placeholder
          addresses that the deployment fills in. This screen goes live with them.
        </div>
      )}

      {deployed && s && <StatusStrip s={s} />}
      {deployed && snapshot.isError && (
        <div className="dash-alert">
          Spoke RPC unreachable: {snapshot.error.message}
        </div>
      )}

      {/* ── Deposit ─────────────────────────────────────────────────── */}
      <section className="admin-section">
        <div className="admin-section__head">
          <h2 className="admin-section__title">Deposit</h2>
          <div className="admin-section__sub">
            Escrows USDG on {cfg.chainName} as <em>pending</em>; it becomes part of the
            vault when the hub ACKs (typically one hub crank, minutes not hours). No
            ACK within {s ? Math.round(s.depositTimeoutSecs / 3600) : 24}h → reclaim
            your escrow below.
          </div>
        </div>

        <div className="bridge-form">
          <div className="bridge-form__field">
            <span>
              Amount (USDG)
              {user && ` — balance ${fmtUsdg(user.usdgBalanceRaw, cfg.usdgDecimals)}`}
            </span>
            <input
              type="text"
              inputMode="decimal"
              placeholder="0.00"
              value={depositAmount}
              onChange={(e) => setDepositAmount(e.target.value)}
              disabled={busy !== null || !deployed}
            />
          </div>

          <div className="bridge-form__field">
            <span>Tranche</span>
            <div className="bridge-form__direction">
              {TRANCHE_LABEL.map((label, t) => (
                <button
                  key={label}
                  className={depositTranche === t ? "is-active" : ""}
                  onClick={() => setDepositTranche(t)}
                  disabled={busy !== null}
                >
                  {label}
                </button>
              ))}
            </div>
          </div>

          <div className="bridge-form__actions">
            {connectBtn}
            {ENV === "testnet" && account.address && (
              <button
                className="bridge-form__secondary"
                disabled={busy !== null || !deployed}
                onClick={() =>
                  run("faucet", "spoke_faucet_minted", `minted ${FAUCET_UNITS} TUSDG`, () =>
                    mintTusdg(account.address!, parseUnits(FAUCET_UNITS, cfg.usdgDecimals)),
                  )
                }
              >
                {busy === "faucet" ? "minting…" : `Faucet ${FAUCET_UNITS} TUSDG`}
              </button>
            )}
            <button
              className="bridge-form__submit"
              disabled={busy !== null || !canDeposit}
              onClick={() =>
                // Approve-then-deposit in one click: exact-amount approval
                // first when the standing allowance is short, then deposit.
                run(
                  "deposit",
                  "spoke_deposit",
                  `deposited ${depositAmount} USDG (${TRANCHE_LABEL[depositTranche]})`,
                  async () => {
                    if (needsApproval) await approveUsdg(account.address!, depositRaw);
                    await depositUsdg(account.address!, depositRaw, depositTranche);
                    setDepositAmount("");
                  },
                )
              }
            >
              {busy === "deposit"
                ? needsApproval
                  ? "approving + depositing…"
                  : "depositing…"
                : needsApproval
                  ? "Approve + deposit"
                  : "Deposit"}
            </button>
            {s?.paused && <span className="bridge-form__hint">deposits are paused</span>}
          </div>
        </div>

        {user && user.deposits.length > 0 && (
          <DepositsTable
            deposits={user.deposits}
            decimals={cfg.usdgDecimals}
            busy={busy}
            onReclaim={(d) =>
              run("reclaim-" + d.seq, "spoke_deposit_reclaimed", `reclaimed deposit #${d.seq}`, () =>
                reclaimDeposit(account.address!, d.seq),
              )
            }
          />
        )}
      </section>

      {/* ── Withdraw ────────────────────────────────────────────────── */}
      <section className="admin-section">
        <div className="admin-section__head">
          <h2 className="admin-section__title">Withdraw</h2>
          <div className="admin-section__sub">
            Requests are share-denominated; the hub prices them at NAV, burns the
            shares and directs payment in USDG — paid instantly when the spoke holds
            enough, queued FIFO otherwise. One in-flight request per tranche. Share
            counts below are the spoke&apos;s non-authoritative mirror of the hub ledger.
          </div>
        </div>

        {!account.address ? (
          <div className="bridge-form__actions">{connectBtn}</div>
        ) : !user ? (
          <div className="admin-empty">
            {deployed ? "Loading your position…" : "Awaiting spoke deployment."}
          </div>
        ) : (
          <>
            <div className="admin-table">
              <div className="admin-table__head admin-table__row">
                <span>Tranche</span>
                <span>Shares (mirror)</span>
                <span>In-flight request</span>
              </div>
              {user.tranches.map((t) => (
                <div className="admin-table__row" key={t.tranche}>
                  <span>{TRANCHE_LABEL[t.tranche]}</span>
                  <span>{fmtShares(t.mirrorShares)}</span>
                  <span>
                    {t.inFlight ? (
                      <span className="admin-tag admin-tag--mute">
                        #{t.inFlight.seq.toString()} ·{" "}
                        {t.inFlight.all ? "all shares" : `${fmtShares(t.inFlight.shares)} shares`} ·
                        awaiting hub ACK
                      </span>
                    ) : (
                      <span className="admin-cell__dim">none</span>
                    )}
                  </span>
                </div>
              ))}
            </div>

            <div className="bridge-form" style={{ marginTop: 14 }}>
              <div className="bridge-form__field">
                <span>Tranche</span>
                <div className="bridge-form__direction">
                  {TRANCHE_LABEL.map((label, t) => (
                    <button
                      key={label}
                      className={withdrawTranche === t ? "is-active" : ""}
                      onClick={() => setWithdrawTranche(t)}
                      disabled={busy !== null}
                    >
                      {label}
                    </button>
                  ))}
                </div>
              </div>
              <div className="bridge-form__field">
                <span>Shares</span>
                <div className="bridge-form__direction">
                  <button
                    className={!withdrawAll ? "is-active" : ""}
                    onClick={() => setWithdrawAll(false)}
                    disabled={busy !== null}
                  >
                    Amount
                  </button>
                  <button
                    className={withdrawAll ? "is-active" : ""}
                    onClick={() => setWithdrawAll(true)}
                    disabled={busy !== null}
                  >
                    All
                  </button>
                </div>
                {!withdrawAll && (
                  <input
                    type="text"
                    inputMode="numeric"
                    placeholder="share count"
                    value={withdrawShares}
                    onChange={(e) => setWithdrawShares(e.target.value)}
                    disabled={busy !== null}
                  />
                )}
              </div>
              <div className="bridge-form__actions">
                <button
                  className="bridge-form__submit"
                  disabled={busy !== null || !canWithdraw}
                  onClick={() =>
                    run(
                      "withdraw",
                      "spoke_withdraw_requested",
                      `withdraw requested (${TRANCHE_LABEL[withdrawTranche]})`,
                      async () => {
                        await requestWithdraw(
                          account.address!,
                          withdrawTranche,
                          withdrawAll ? 0n : withdrawSharesRaw,
                          withdrawAll,
                        );
                        setWithdrawShares("");
                        setWithdrawAll(false);
                      },
                    )
                  }
                >
                  {busy === "withdraw" ? "requesting…" : "Request withdraw"}
                </button>
                {withdrawRow?.inFlight && (
                  <span className="bridge-form__hint">
                    request #{withdrawRow.inFlight.seq.toString()} already in flight for this
                    tranche
                  </span>
                )}
              </div>
            </div>
          </>
        )}
      </section>

      {/* ── Payout queue ────────────────────────────────────────────── */}
      {deployed && s && (
        <section className="admin-section">
          <div className="admin-section__head">
            <h2 className="admin-section__title">Payout queue</h2>
            <div className="admin-section__sub">
              Hub-ACK&apos;d withdrawals the spoke can&apos;t yet fully pay, serviced FIFO
              from incoming funds. Processing is permissionless.
            </div>
          </div>
          {s.queueDepth === 0n ? (
            <div className="admin-empty">Queue is empty — payouts settle instantly.</div>
          ) : (
            <>
              <div className="admin-table">
                <div className="admin-table__head admin-table__row">
                  <span>Position</span>
                  <span>Request</span>
                  <span>Owed (USDG)</span>
                  <span>Funded</span>
                </div>
                {s.queue.map((q, i) => {
                  const mine =
                    account.address !== null &&
                    q.user.toLowerCase() === account.address.toLowerCase();
                  return (
                    <div className="admin-table__row" key={q.index.toString()}>
                      <span>
                        #{i + 1} of {s.queueDepth.toString()}
                        {mine && (
                          <span className="admin-tag admin-tag--ok" style={{ marginLeft: 6 }}>
                            you
                          </span>
                        )}
                      </span>
                      <span>#{q.requestSeq.toString()}</span>
                      <span>{fmtUsdg(q.owedRaw, cfg.usdgDecimals)}</span>
                      <span>
                        {fmtUsdg(q.reservedRaw, cfg.usdgDecimals)} /{" "}
                        {fmtUsdg(q.owedRaw, cfg.usdgDecimals)}
                      </span>
                    </div>
                  );
                })}
              </div>
              <div className="bridge-form__actions" style={{ marginTop: 12 }}>
                <button
                  className="admin-btn admin-btn--primary"
                  disabled={busy !== null || !account.address || s.paused}
                  onClick={() =>
                    run("process-queue", "spoke_queue_processed", "payout queue processed", () =>
                      processPayoutQueue(account.address!),
                    )
                  }
                >
                  {busy === "process-queue" ? "processing…" : "Process payout queue"}
                </button>
                {s.funds.active === 0n && (
                  <span className="bridge-form__hint">
                    no active funds to drain the queue with — it fills from new deposits
                  </span>
                )}
              </div>
            </>
          )}
        </section>
      )}

      {toast && <Toast message={toast.message} variant={toast.variant} />}
    </div>
  );
}

function Hero({ sub }: { sub: string }) {
  return (
    <div className="dash-hero">
      <div className="dash-hero__eyebrow">multichain · spoke vault</div>
      <h1 className="dash-hero__title">Spoke</h1>
      <div className="dash-hero__addr">{sub}</div>
    </div>
  );
}

function StatusStrip({ s }: { s: SpokeVaultSnapshot }) {
  const cfg = SPOKE_CONFIG!;
  const nowSecs = Math.floor(Date.now() / 1000);
  const heartbeatAge = nowSecs - s.lastInboundAt;
  const feePotLow = s.feePotWei < FEE_POT_LOW_WEI;
  return (
    <>
      <div className="dash-summary">
        <div className="dash-summary__cell">
          <div className="dash-summary__label">Spoke status</div>
          <div className="dash-summary__val">
            {s.paused ? (
              <span className="admin-tag admin-tag--danger">paused</span>
            ) : s.effectiveRiskOff ? (
              <span className="admin-tag admin-tag--danger">risk off</span>
            ) : (
              <span className="admin-tag admin-tag--ok">open</span>
            )}
          </div>
          <div className="dash-summary__sub">
            {s.paused
              ? "deposits and payouts halted"
              : s.heartbeatStale
                ? "heartbeat stale — integrations frozen; deposits/payouts continue"
                : s.riskOff
                  ? "hub risk_off — integrations frozen"
                  : "all lanes normal"}
          </div>
        </div>
        <div className="dash-summary__cell">
          <div className="dash-summary__label">Hub heartbeat</div>
          <div className="dash-summary__val">{fmtAgo(heartbeatAge)}</div>
          <div className="dash-summary__sub">
            last inbound hub message · stale after{" "}
            {Math.round(s.heartbeatTimeoutSecs / 60)}m of silence
          </div>
        </div>
        <div className="dash-summary__cell">
          <div className="dash-summary__label">Message fee pot</div>
          <div className="dash-summary__val">
            {fmtEth(s.feePotWei)} <span className="dash-summary__unit">ETH</span>
          </div>
          <div className="dash-summary__sub">
            {feePotLow
              ? "low — deposits/withdrawals may fail to message the hub"
              : "pays hub message transport"}
          </div>
        </div>
        <div className="dash-summary__cell">
          <div className="dash-summary__label">Vault funds</div>
          <div className="dash-summary__val">
            {fmtUsdg(s.funds.pending + s.funds.active + s.funds.reserved, cfg.usdgDecimals)}
            <span className="dash-summary__unit"> USDG</span>
          </div>
          <div className="dash-summary__sub">
            {fmtUsdg(s.funds.pending, cfg.usdgDecimals)} pending ·{" "}
            {fmtUsdg(s.funds.active, cfg.usdgDecimals)} active ·{" "}
            {fmtUsdg(s.funds.reserved, cfg.usdgDecimals)} reserved
          </div>
        </div>
      </div>
      {(s.paused || s.effectiveRiskOff || feePotLow) && (
        <div className="dash-alert">
          {s.paused && "The spoke is paused — deposits and payouts revert. "}
          {s.heartbeatStale &&
            `No hub message for ${fmtAgo(heartbeatAge).replace(" ago", "")} — hub ACKs (deposit activation, withdrawal pricing) are likely delayed. `}
          {!s.heartbeatStale &&
            s.riskOff &&
            "Hub risk_off is set — curator integrations are frozen. "}
          {feePotLow &&
            `Fee pot is low (${fmtEth(s.feePotWei)} ETH) — outbound hub messages can fail until it is topped up.`}
        </div>
      )}
    </>
  );
}

function DepositsTable({
  deposits,
  decimals,
  busy,
  onReclaim,
}: {
  deposits: SpokeDepositRow[];
  decimals: number;
  busy: string | null;
  onReclaim: (d: SpokeDepositRow) => void;
}) {
  return (
    <div className="admin-table" style={{ marginTop: 14 }}>
      <div className="admin-table__head admin-table__row">
        <span>Deposit</span>
        <span>Amount (USDG)</span>
        <span>Tranche</span>
        <span>Status</span>
        <span>Action</span>
      </div>
      {deposits.map((d) => {
        const statusTag =
          d.status === DEPOSIT_STATUS.Acked
            ? "admin-tag--ok"
            : d.status === DEPOSIT_STATUS.Pending
              ? "admin-tag--mute"
              : "admin-tag--danger";
        return (
          <div className="admin-table__row" key={d.seq.toString()}>
            <span>
              #{d.seq.toString()}
              <code className="admin-cell__id">
                {new Date(d.ts * 1000).toLocaleString()}
              </code>
            </span>
            <span>{fmtUsdg(d.amountRaw, decimals)}</span>
            <span>{TRANCHE_LABEL[d.tranche] ?? d.tranche}</span>
            <span>
              <span className={`admin-tag ${statusTag}`}>
                {DEPOSIT_STATUS_LABEL[d.status] ?? d.status}
              </span>
              {d.status === DEPOSIT_STATUS.Pending && !d.reclaimable && (
                <span className="admin-cell__dim" style={{ marginLeft: 6 }}>
                  awaiting hub ACK
                </span>
              )}
            </span>
            <span className="admin-cell__actions">
              {d.reclaimable && (
                <button
                  className="admin-btn admin-btn--danger"
                  disabled={busy !== null}
                  onClick={() => onReclaim(d)}
                >
                  {busy === "reclaim-" + d.seq ? "reclaiming…" : "Reclaim"}
                </button>
              )}
            </span>
          </div>
        );
      })}
    </div>
  );
}
