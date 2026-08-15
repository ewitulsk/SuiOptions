// Admin console. Visible only to wallets that hold an `AdminCap`
// (`useAdminCap`); the nav link in `Header` is gated by the same check and
// non-admins hitting `/admin` directly are redirected to `/earn`.
//
// Surfaces every admin-gated contract entrypoint:
//   - access control: ingress whitelist + big red button   (standalone
//     whitelist.move + trading-vault/exchange registry.move pause legs)
//   - per-bucket invalidate / revalidate / cleanup        (bucket.move)
//   - set protocol fee (set_fee_bps)                       (admin.move)
//   - withdraw treasury fees / (re)create the treasury     (treasury.move)
import { optionCoinType } from "../api/client";

import { useMemo, useState } from "react";
import { Navigate } from "react-router-dom";
import { useCurrentAccount } from "@mysten/dapp-kit";
import type { Transaction } from "@mysten/sui/transactions";
import { isValidSuiAddress, normalizeSuiAddress } from "@mysten/sui/utils";
import { useSubmitTransaction } from "../tx/submit";

import { Toast } from "../components/Toast";
import { TokenManager } from "../components/TokenManager";
import { useBuckets } from "../api/useBuckets";
import { useAdminCap } from "../api/useAdminCap";
import { useWhitelist } from "../api/useWhitelist";
import type { Bucket, Series } from "../api/client";
import {
  buildCleanupBucketTx,
  buildCreateTreasuryTx,
  buildInvalidateBucketTx,
  buildPauseIngressTx,
  buildRevalidateBucketTx,
  buildSetFeeBpsTx,
  buildSetWhitelistEnabledTx,
  buildUnpauseIngressTx,
  buildWhitelistAddTx,
  buildWhitelistRemoveTx,
  buildWithdrawTx,
  type IngressPauseParams,
  type IngressWhitelistParams,
} from "../tx/admin";
import {
  EXCHANGE_MARKETS,
  PROTOCOL_CONFIG_ID,
  TRADING_VAULT_OBJECTS,
  TREASURY_ID,
  WHITELIST_ID,
} from "../config";

function scaleRaw(raw: string, decimals: number | null): string {
  if (decimals === null) return raw;
  const v = Number(BigInt(raw)) / 10 ** decimals;
  return v.toLocaleString("en-US", { maximumFractionDigits: 6 });
}

type FlatBucket = {
  series: Series;
  /** Always a created bucket — admin actions all take an object id. */
  bucket: Bucket & { bucket_id: string };
  expired: boolean;
};

export function Admin() {
  const account = useCurrentAccount();
  const wallet = account?.address ?? null;
  const adminCap = useAdminCap(wallet);
  // Admin is the monitoring surface: it wants every bucket ever created, not
  // the listed board, and never the not-yet-created strikes the board adds
  // (SO-400) — there is nothing to invalidate or clean on those.
  const buckets = useBuckets({ all: true });
  const whitelist = useWhitelist();
  const submitTx = useSubmitTransaction();

  const [toast, setToast] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [reason, setReason] = useState("");

  const now = Date.now();
  const flat = useMemo<FlatBucket[]>(() => {
    const out: FlatBucket[] = [];
    for (const series of buckets.data ?? []) {
      for (const bucket of series.buckets) {
        if (bucket.bucket_id === null) continue;
        out.push({
          series,
          bucket: bucket as Bucket & { bucket_id: string },
          expired: series.expiry_ms < now,
        });
      }
    }
    return out;
  }, [buckets.data, now]);

  // Wallet must be connected and admin. While the cap query resolves we
  // show a spinner rather than flashing a redirect.
  if (!wallet) return <Navigate to="/earn" replace />;
  if (adminCap.isLoading) {
    return (
      <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
        <div className="app__wrap">
          <div className="admin-gate">checking admin access…</div>
        </div>
      </div>
    );
  }
  if (!adminCap.data?.isAdmin || !adminCap.data.adminCapId) {
    return <Navigate to="/earn" replace />;
  }
  const adminCapId = adminCap.data.adminCapId;
  const exchangeAdminCapId = adminCap.data.exchangeAdminCapId;
  const whitelistAdminCapId = adminCap.data.whitelistAdminCapId;

  const flash = (msg: string) => {
    setToast(msg);
    setTimeout(() => setToast(null), 5000);
  };

  // Sign + execute one PTB, tracking a `busy` key so individual buttons can
  // show their own pending state and disable while in flight. Returns
  // whether the submit succeeded (access control clears its input on it).
  const run = async (key: string, build: () => Transaction, ok: string): Promise<boolean> => {
    setBusy(key);
    try {
      const tx = build();
      await submitTx(tx);
      flash(`✓ ${ok}`);
      buckets.refetch();
      whitelist.refetch();
      return true;
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      flash(`failed · ${message}`);
      return false;
    } finally {
      setBusy(null);
    }
  };

  return (
    <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
      <div className="app__wrap">
        <div className="dash-hero">
          <div className="dash-hero__eyebrow">privileged · AdminCap holder</div>
          <h1 className="dash-hero__title">Admin</h1>
          <div className="dash-hero__addr">
            cap {adminCapId.slice(0, 6)}…{adminCapId.slice(-4)}
          </div>
        </div>

        {/* ── Access control ──────────────────────────────────────── */}
        <AccessControl
          busy={busy}
          run={run}
          adminCapId={adminCapId}
          exchangeAdminCapId={exchangeAdminCapId}
          whitelistAdminCapId={whitelistAdminCapId}
          whitelist={whitelist}
        />

        {/* ── Buckets ─────────────────────────────────────────────── */}
        <section className="admin-section">
          <div className="admin-section__head">
            <h2 className="admin-section__title">Call options &amp; buckets</h2>
            <div className="admin-section__sub">
              every on-chain bucket · invalidate freezes new writes
              (exercise/redeem unaffected)
            </div>
          </div>

          <div className="admin-reason">
            <label className="admin-field__label">
              Reason (recorded on invalidate / revalidate)
            </label>
            <input
              className="admin-field__input"
              placeholder="e.g. mispriced strike — pausing new writes"
              value={reason}
              onChange={(e) => setReason(e.target.value)}
            />
          </div>

          {buckets.isLoading && <div className="admin-empty">loading buckets…</div>}
          {buckets.error && (
            <div className="admin-empty">failed to load buckets · {buckets.error.message}</div>
          )}
          {!buckets.isLoading && flat.length === 0 && (
            <div className="admin-empty">no buckets on chain yet.</div>
          )}

          {flat.length > 0 && (
            <div className="admin-table">
              <div className="admin-table__head admin-table__row">
                <span>Asset</span>
                <span>Strike</span>
                <span>Expiry</span>
                <span>Written / Exercised</span>
                <span>Status</span>
                <span>Actions</span>
              </div>
              {flat.map(({ series, bucket, expired }) => {
                const key = bucket.bucket_id;
                return (
                  <div className="admin-table__row" key={key}>
                    <span className="admin-cell__asset">
                      {series.asset_symbol}/{series.settlement_symbol}
                      <code className="admin-cell__id">
                        {bucket.bucket_id.slice(0, 6)}…{bucket.bucket_id.slice(-4)}
                      </code>
                    </span>
                    <span>
                      {bucket.strike ?? bucket.strike_raw}
                      <span className="admin-cell__dim"> ×10⁻{bucket.strike_scale}</span>
                    </span>
                    <span>
                      {series.expiry_iso.slice(0, 10)}
                      {expired && <span className="admin-tag admin-tag--mute"> expired</span>}
                    </span>
                    <span>
                      {scaleRaw(bucket.total_written_raw, series.asset_decimals)} /{" "}
                      {scaleRaw(bucket.exercise_cursor_raw, series.asset_decimals)}
                    </span>
                    <span>
                      {bucket.invalidated ? (
                        <span className="admin-tag admin-tag--danger">invalidated</span>
                      ) : (
                        <span className="admin-tag admin-tag--ok">active</span>
                      )}
                    </span>
                    <span className="admin-cell__actions">
                      {!expired && !bucket.invalidated && (
                        <button
                          className="admin-btn admin-btn--danger"
                          disabled={busy !== null}
                          onClick={() =>
                            run(
                              `inv-${key}`,
                              () =>
                                buildInvalidateBucketTx({
                                  adminCapId,
                                  bucketId: bucket.bucket_id,
                                  underlyingCoinType: series.asset_coin_type,
                                  settlementCoinType: series.settlement_coin_type,
                                  callCoinType: optionCoinType(bucket),
                                  reason,
                                }),
                              "bucket invalidated",
                            )
                          }
                        >
                          {busy === `inv-${key}` ? "…" : "Invalidate"}
                        </button>
                      )}
                      {!expired && bucket.invalidated && (
                        <button
                          className="admin-btn"
                          disabled={busy !== null}
                          onClick={() =>
                            run(
                              `rev-${key}`,
                              () =>
                                buildRevalidateBucketTx({
                                  adminCapId,
                                  bucketId: bucket.bucket_id,
                                  underlyingCoinType: series.asset_coin_type,
                                  settlementCoinType: series.settlement_coin_type,
                                  callCoinType: optionCoinType(bucket),
                                  reason,
                                }),
                              "bucket revalidated",
                            )
                          }
                        >
                          {busy === `rev-${key}` ? "…" : "Revalidate"}
                        </button>
                      )}
                      {expired && (
                        <button
                          className="admin-btn"
                          disabled={busy !== null}
                          title="Only succeeds once the bucket's balances are fully drained"
                          onClick={() =>
                            run(
                              `cln-${key}`,
                              () =>
                                buildCleanupBucketTx({
                                  adminCapId,
                                  bucketId: bucket.bucket_id,
                                  underlyingCoinType: series.asset_coin_type,
                                  settlementCoinType: series.settlement_coin_type,
                                  callCoinType: optionCoinType(bucket),
                                }),
                              "bucket cleaned up",
                            )
                          }
                        >
                          {busy === `cln-${key}` ? "…" : "Cleanup"}
                        </button>
                      )}
                    </span>
                  </div>
                );
              })}
            </div>
          )}
        </section>

        {/* The covered-call Vaults panel (pause/unpause deposits) was
            removed with that product — SO-332. The curated trading vaults
            are managed from the curator screens, not here. */}

        {/* ── Protocol fee ────────────────────────────────────────── */}
        <SetFeeForm
          busy={busy}
          onSubmit={(build) => run("set-fee", build, "fee updated")}
          adminCapId={adminCapId}
        />

        {/* ── Treasury ────────────────────────────────────────────── */}
        <TreasuryForms
          busy={busy}
          onWithdraw={(build) => run("withdraw", build, "treasury withdrawn")}
          onCreate={(build) => run("create-treasury", build, "treasury created")}
          adminCapId={adminCapId}
        />

        {/* ── Supported tokens (token-info) ───────────────────────── */}
        <TokenManager flash={flash} />
      </div>

      {toast && <Toast message={toast} />}
    </div>
  );
}

// ── Access control (guarded launch) ────────────────────────────────────
//
// ONE standalone whitelist package (`whitelist::whitelist`): one shared
// `Whitelist` object gating ingress across core / trading-vault / exchange,
// mutated with its own `whitelist::AdminCap`. The big-red-button pause
// additionally flips the trading-vault and per-market exchange registry
// pause flags (core / exchange caps) in the same PTB.

const memberGrid = { gridTemplateColumns: "3.2fr 0.8fr" };

const MSG_GO_PUBLIC =
  "Disable whitelist enforcement (go public)?\n\n" +
  "Anyone will be able to deposit, write, and fill — protocol-wide. " +
  "Membership is retained on-chain, so re-enabling restores the current cohort.";
const MSG_ENFORCE =
  "Enforce the whitelist?\n\n" +
  "Only listed members will be able to deposit, write, and fill — protocol-wide. " +
  "Exits (withdrawals, cancels, exercises) are never gated.";
const MSG_PAUSE =
  "PAUSE ALL INGRESS?\n\n" +
  "This blocks ALL deposits, writes, and fills protocol-wide: the ingress whitelist, " +
  "the trading vaults, and every exchange market. " +
  "Exits (withdrawals, cancels, exercises) stay open — nobody is stranded.";
const MSG_UNPAUSE =
  "Unpause ingress?\n\n" +
  "Deposits, writes, and fills resume protocol-wide (the whitelist, the trading vaults, " +
  "and every exchange market), subject to whitelist enforcement.";

function AccessControl({
  busy,
  run,
  adminCapId,
  exchangeAdminCapId,
  whitelistAdminCapId,
  whitelist,
}: {
  busy: string | null;
  run: (key: string, build: () => Transaction, ok: string) => Promise<boolean>;
  adminCapId: string;
  exchangeAdminCapId: string | null;
  whitelistAdminCapId: string | null;
  whitelist: ReturnType<typeof useWhitelist>;
}) {
  const [addr, setAddr] = useState("");

  const wl = whitelist.data ?? null;
  const configMissing = !WHITELIST_ID;
  const missingCap = !configMissing && !whitelistAdminCapId;
  const canMutate = !configMissing && !missingCap && wl != null;

  const ids = (): IngressWhitelistParams => ({
    whitelistAdminCapId: whitelistAdminCapId as string,
    whitelistId: WHITELIST_ID as string,
  });
  const pauseIds = (): IngressPauseParams => ({
    ...ids(),
    coreAdminCapId: adminCapId,
    vaultProtocolConfigId: TRADING_VAULT_OBJECTS?.vaultProtocolConfigId ?? null,
    exchangeAdminCapId,
    markets: EXCHANGE_MARKETS,
  });

  const members = [...(wl?.members ?? [])].sort((a, b) => a.localeCompare(b));

  const trimmed = addr.trim();
  const addrValid =
    /^0x[0-9a-fA-F]{1,64}$/.test(trimmed) && isValidSuiAddress(normalizeSuiAddress(trimmed));
  const normalized = addrValid ? normalizeSuiAddress(trimmed) : null;
  // VecSet insert aborts on duplicates — block re-adding an existing member.
  const addable = normalized !== null && !members.includes(normalized);

  const paused = !!wl?.ingressPaused;
  const enforced = !!wl?.whitelistEnabled;
  const status = paused ? "PAUSED" : enforced ? "GATED" : "OPEN";
  const statusClass =
    status === "PAUSED"
      ? "admin-tag--danger"
      : status === "GATED"
        ? "admin-tag--ok"
        : "admin-tag--mute";

  const addMember = async () => {
    if (!normalized || !addable) return;
    const ok = await run(
      "wl-add",
      () => buildWhitelistAddTx(ids(), normalized),
      "member added",
    );
    if (ok) setAddr("");
  };

  return (
    <section className="admin-section">
      <div className="admin-section__head">
        <h2 className="admin-section__title">Access control</h2>
        <div className="admin-section__sub">
          guarded-launch ingress whitelist · one list gates the whole protocol ·
          exits are never gated
        </div>
      </div>

      {/* status banner */}
      <div className="admin-actions-row" style={{ alignItems: "center", marginBottom: 14 }}>
        <span className={`admin-tag ${statusClass}`}>{status}</span>
        {whitelist.isLoading && <span className="admin-cell__dim">loading whitelist state…</span>}
        {whitelist.error && (
          <span className="admin-cell__dim">
            failed to read whitelist · {whitelist.error.message}
          </span>
        )}
        {paused && (
          <span className="admin-cell__dim">
            all deposits/writes/fills blocked · exits stay open
          </span>
        )}
      </div>

      {configMissing && (
        <div className="admin-empty">
          No whitelist is deployed for this environment — cannot target the
          shared <code>Whitelist</code>.
        </div>
      )}
      {missingCap && (
        <div className="admin-empty">
          This wallet does not hold the <code>whitelist::AdminCap</code> —
          access-control changes are disabled.
        </div>
      )}

      {/* member table */}
      {!whitelist.isLoading && members.length === 0 && (
        <div className="admin-empty">no whitelisted members yet.</div>
      )}
      {members.length > 0 && (
        <div className="admin-table">
          <div className="admin-table__head admin-table__row" style={memberGrid}>
            <span>Member</span>
            <span>Actions</span>
          </div>
          {members.map((member) => (
            <div className="admin-table__row" style={memberGrid} key={member}>
              <code className="admin-cell__id" style={{ fontSize: 12 }}>
                {member}
              </code>
              <span className="admin-cell__actions">
                <button
                  className="admin-btn admin-btn--danger"
                  disabled={!canMutate || busy !== null}
                  onClick={() =>
                    run(
                      `wl-rm-${member}`,
                      () => buildWhitelistRemoveTx(ids(), member),
                      "member removed",
                    )
                  }
                >
                  {busy === `wl-rm-${member}` ? "…" : "Remove"}
                </button>
              </span>
            </div>
          ))}
        </div>
      )}

      {/* add member */}
      <div className="admin-grid" style={{ marginTop: 12 }}>
        <Field label="Add address">
          <input
            className="admin-field__input"
            placeholder="0x…"
            value={addr}
            onChange={(e) => setAddr(e.target.value)}
          />
        </Field>
      </div>
      <div className="admin-actions-row">
        <button
          className="admin-btn admin-btn--primary"
          disabled={!canMutate || busy !== null || !addable}
          title={
            trimmed && !addrValid
              ? "not a valid 0x address"
              : normalized && !addable
                ? "already a member"
                : undefined
          }
          onClick={addMember}
        >
          {busy === "wl-add" ? "adding…" : "Add member"}
        </button>
      </div>

      {/* levers */}
      <div className="admin-actions-row" style={{ marginTop: 18 }}>
        <button
          className="admin-btn"
          disabled={!canMutate || busy !== null}
          title="Go-public lever: flips whitelist enforcement"
          onClick={() => {
            if (!window.confirm(enforced ? MSG_GO_PUBLIC : MSG_ENFORCE)) return;
            run(
              "wl-enabled",
              () => buildSetWhitelistEnabledTx(ids(), !enforced),
              enforced ? "whitelist disabled — protocol is public" : "whitelist enforced",
            );
          }}
        >
          {busy === "wl-enabled"
            ? "…"
            : enforced
              ? "Disable whitelist (go public)"
              : "Enforce whitelist"}
        </button>
        <button
          className={paused ? "admin-btn" : "admin-btn admin-btn--danger"}
          disabled={!canMutate || busy !== null}
          title="Big red button: pauses all ingress protocol-wide; exits stay open"
          onClick={() => {
            if (!window.confirm(paused ? MSG_UNPAUSE : MSG_PAUSE)) return;
            run(
              "wl-paused",
              () => (paused ? buildUnpauseIngressTx(pauseIds()) : buildPauseIngressTx(pauseIds())),
              paused ? "ingress unpaused" : "ingress paused protocol-wide",
            );
          }}
        >
          {busy === "wl-paused" ? "…" : paused ? "Unpause ingress" : "Pause all ingress"}
        </button>
      </div>
    </section>
  );
}

// ── Protocol fee ───────────────────────────────────────────────────────

function SetFeeForm({
  busy,
  onSubmit,
  adminCapId,
}: {
  busy: string | null;
  onSubmit: (build: () => Transaction) => void;
  adminCapId: string;
}) {
  const [bps, setBps] = useState("0");
  const configMissing = !PROTOCOL_CONFIG_ID;

  return (
    <section className="admin-section">
      <div className="admin-section__head">
        <h2 className="admin-section__title">Protocol fee</h2>
        <div className="admin-section__sub">
          basis points · max 1000 (10%). e.g. 50 = 0.5%.
        </div>
      </div>
      {configMissing && (
        <div className="admin-empty">
          No deployment is configured for this environment — cannot target
          the shared <code>ProtocolConfig</code>.
        </div>
      )}
      <div className="admin-grid">
        <Field label="Fee (bps)">
          <input
            className="admin-field__input"
            type="number"
            min={0}
            max={1000}
            value={bps}
            onChange={(e) => setBps(e.target.value)}
          />
        </Field>
      </div>
      <button
        className="admin-btn admin-btn--primary"
        disabled={configMissing || busy !== null}
        onClick={() =>
          onSubmit(() =>
            buildSetFeeBpsTx({
              adminCapId,
              protocolConfigId: PROTOCOL_CONFIG_ID as string,
              newBps: BigInt(bps || "0"),
            }),
          )
        }
      >
        {busy === "set-fee" ? "updating…" : "Set fee"}
      </button>
    </section>
  );
}

// ── Treasury ───────────────────────────────────────────────────────────

function TreasuryForms({
  busy,
  onWithdraw,
  onCreate,
  adminCapId,
}: {
  busy: string | null;
  onWithdraw: (build: () => Transaction) => void;
  onCreate: (build: () => Transaction) => void;
  adminCapId: string;
}) {
  const [coinType, setCoinType] = useState("");
  const [amount, setAmount] = useState("");
  const [recipient, setRecipient] = useState("");
  const treasuryMissing = !TREASURY_ID;

  const ready = coinType && amount && recipient;

  return (
    <section className="admin-section">
      <div className="admin-section__head">
        <h2 className="admin-section__title">Treasury</h2>
        <div className="admin-section__sub">
          withdraw accrued protocol fees · amount in the coin's smallest units.
        </div>
      </div>
      {treasuryMissing && (
        <div className="admin-empty">
          No deployment is configured for this environment — cannot target
          the shared <code>Treasury</code>.
        </div>
      )}
      <div className="admin-grid">
        <Field label="Coin type">
          <input
            className="admin-field__input"
            placeholder="0x…::tusdc::TUSDC"
            value={coinType}
            onChange={(e) => setCoinType(e.target.value.trim())}
          />
        </Field>
        <Field label="Amount (raw)">
          <input
            className="admin-field__input"
            type="number"
            min={0}
            value={amount}
            onChange={(e) => setAmount(e.target.value)}
          />
        </Field>
        <Field label="Recipient address">
          <input
            className="admin-field__input"
            placeholder="0x…"
            value={recipient}
            onChange={(e) => setRecipient(e.target.value.trim())}
          />
        </Field>
      </div>
      <div className="admin-actions-row">
        <button
          className="admin-btn admin-btn--primary"
          disabled={treasuryMissing || !ready || busy !== null}
          onClick={() =>
            onWithdraw(() =>
              buildWithdrawTx({
                adminCapId,
                treasuryId: TREASURY_ID as string,
                coinType,
                amountRaw: BigInt(amount || "0"),
                recipient,
              }),
            )
          }
        >
          {busy === "withdraw" ? "withdrawing…" : "Withdraw"}
        </button>
        <button
          className="admin-btn"
          disabled={busy !== null}
          title="Shares a fresh Treasury object. Only needed if none exists."
          onClick={() => onCreate(() => buildCreateTreasuryTx(adminCapId))}
        >
          {busy === "create-treasury" ? "creating…" : "Create treasury"}
        </button>
      </div>
    </section>
  );
}

function Field({
  label,
  children,
}: {
  label: string;
  children: React.ReactNode;
}) {
  return (
    <div className="admin-field">
      <label className="admin-field__label">{label}</label>
      {children}
    </div>
  );
}
