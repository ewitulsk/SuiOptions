// Admin console. Visible only to wallets that hold an `AdminCap`
// (`useAdminCap`); the nav link in `Header` is gated by the same check and
// non-admins hitting `/admin` directly are redirected to `/earn`.
//
// Surfaces every admin-gated contract entrypoint:
//   - per-bucket invalidate / revalidate / cleanup        (bucket.move)
//   - create a strike ladder of buckets (new_call_option)  (bucket.move)
//   - set protocol fee (set_fee_bps)                       (admin.move)
//   - withdraw treasury fees / (re)create the treasury     (treasury.move)

import { useMemo, useState } from "react";
import { Navigate } from "react-router-dom";
import {
  useCurrentAccount,
  useSignAndExecuteTransaction,
} from "@mysten/dapp-kit";
import type { Transaction } from "@mysten/sui/transactions";

import { Header } from "../components/Header";
import { WaveHero } from "../components/WaveHero";
import { Toast } from "../components/Toast";
import { useBuckets } from "../api/useBuckets";
import { useAdminCap } from "../api/useAdminCap";
import type { Bucket, Series } from "../api/client";
import {
  buildCleanupBucketTx,
  buildCreateTreasuryTx,
  buildInvalidateBucketTx,
  buildNewCallOptionTx,
  buildRevalidateBucketTx,
  buildSetFeeBpsTx,
  buildWithdrawTx,
} from "../tx/admin";

const PROTOCOL_CONFIG_ID = import.meta.env.VITE_PROTOCOL_CONFIG_ID as
  | string
  | undefined;
const TREASURY_ID = import.meta.env.VITE_TREASURY_ID as string | undefined;

function scaleRaw(raw: string, decimals: number | null): string {
  if (decimals === null) return raw;
  const v = Number(BigInt(raw)) / 10 ** decimals;
  return v.toLocaleString("en-US", { maximumFractionDigits: 6 });
}

type FlatBucket = {
  series: Series;
  bucket: Bucket;
  expired: boolean;
};

export function Admin() {
  const account = useCurrentAccount();
  const wallet = account?.address ?? null;
  const adminCap = useAdminCap(wallet);
  const buckets = useBuckets();
  const { mutateAsync: signAndExecute } = useSignAndExecuteTransaction();

  const [toast, setToast] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [reason, setReason] = useState("");

  const now = Date.now();
  const flat = useMemo<FlatBucket[]>(() => {
    const out: FlatBucket[] = [];
    for (const series of buckets.data ?? []) {
      for (const bucket of series.buckets) {
        out.push({ series, bucket, expired: series.expiry_ms < now });
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
        <WaveHero />
        <Header />
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

  const flash = (msg: string) => {
    setToast(msg);
    setTimeout(() => setToast(null), 5000);
  };

  // Sign + execute one PTB, tracking a `busy` key so individual buttons can
  // show their own pending state and disable while in flight.
  const run = async (key: string, build: () => Transaction, ok: string) => {
    setBusy(key);
    try {
      const tx = build();
      await signAndExecute({ transaction: tx });
      flash(`✓ ${ok}`);
      buckets.refetch();
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      flash(`failed · ${message}`);
    } finally {
      setBusy(null);
    }
  };

  return (
    <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
      <WaveHero />
      <Header />

      <div className="app__wrap">
        <div className="dash-hero">
          <div className="dash-hero__eyebrow">privileged · AdminCap holder</div>
          <h1 className="dash-hero__title">Admin</h1>
          <div className="dash-hero__addr">
            cap {adminCapId.slice(0, 6)}…{adminCapId.slice(-4)}
          </div>
        </div>

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

        {/* ── Create buckets ──────────────────────────────────────── */}
        <CreateCallOptionForm
          busy={busy}
          onSubmit={(build) => run("new-call", build, "buckets created")}
          adminCapId={adminCapId}
        />

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
      </div>

      {toast && <Toast message={toast} />}
    </div>
  );
}

// ── Create call option (strike ladder) ─────────────────────────────────

function CreateCallOptionForm({
  busy,
  onSubmit,
  adminCapId,
}: {
  busy: string | null;
  onSubmit: (build: () => Transaction) => void;
  adminCapId: string;
}) {
  const [underlying, setUnderlying] = useState("");
  const [settlement, setSettlement] = useState("");
  const [expiry, setExpiry] = useState("");
  const [startStrike, setStartStrike] = useState("");
  const [interval, setInterval] = useState("0");
  const [count, setCount] = useState("1");
  const [scale, setScale] = useState("0");

  const ready =
    underlying && settlement && expiry && startStrike && count && scale !== "";

  return (
    <section className="admin-section">
      <div className="admin-section__head">
        <h2 className="admin-section__title">Create call option</h2>
        <div className="admin-section__sub">
          mints <code>count</code> buckets at <code>start_strike</code>,
          <code>start_strike + interval</code>, … sharing one expiry. Strikes
          are raw scaled units: real ratio = <code>strike / 10^scale</code>.
        </div>
      </div>
      <div className="admin-grid">
        <Field label="Underlying coin type">
          <input
            className="admin-field__input"
            placeholder="0x…::tbtc::TBTC"
            value={underlying}
            onChange={(e) => setUnderlying(e.target.value.trim())}
          />
        </Field>
        <Field label="Settlement coin type">
          <input
            className="admin-field__input"
            placeholder="0x…::tusdc::TUSDC"
            value={settlement}
            onChange={(e) => setSettlement(e.target.value.trim())}
          />
        </Field>
        <Field label="Expiry">
          <input
            className="admin-field__input"
            type="datetime-local"
            value={expiry}
            onChange={(e) => setExpiry(e.target.value)}
          />
        </Field>
        <Field label="Strike scale (10^scale)">
          <input
            className="admin-field__input"
            type="number"
            min={0}
            max={38}
            value={scale}
            onChange={(e) => setScale(e.target.value)}
          />
        </Field>
        <Field label="Start strike (raw)">
          <input
            className="admin-field__input"
            type="number"
            min={0}
            value={startStrike}
            onChange={(e) => setStartStrike(e.target.value)}
          />
        </Field>
        <Field label="Strike interval (raw)">
          <input
            className="admin-field__input"
            type="number"
            min={0}
            value={interval}
            onChange={(e) => setInterval(e.target.value)}
          />
        </Field>
        <Field label="Count">
          <input
            className="admin-field__input"
            type="number"
            min={1}
            value={count}
            onChange={(e) => setCount(e.target.value)}
          />
        </Field>
      </div>
      <button
        className="admin-btn admin-btn--primary"
        disabled={!ready || busy !== null}
        onClick={() =>
          onSubmit(() =>
            buildNewCallOptionTx({
              adminCapId,
              underlyingCoinType: underlying,
              settlementCoinType: settlement,
              expiryMs: BigInt(new Date(expiry).getTime()),
              startStrikeRaw: BigInt(startStrike),
              strikeIntervalRaw: BigInt(interval || "0"),
              count: BigInt(count),
              strikeScale: Number(scale),
            }),
          )
        }
      >
        {busy === "new-call" ? "creating…" : "Create buckets"}
      </button>
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
          <code>VITE_PROTOCOL_CONFIG_ID</code> is not set — cannot target the
          shared <code>ProtocolConfig</code>.
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
          <code>VITE_TREASURY_ID</code> is not set — cannot target the shared{" "}
          <code>Treasury</code>.
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
