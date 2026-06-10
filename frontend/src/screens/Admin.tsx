// Admin console. Serves two authority levels:
//
//   - **Platform admin** — wallet holds the protocol `AdminCap`
//     (`useAdminCap`): protocol fee, pause, treasury, supported-token
//     catalog, the verified-orgs allowlist, and emergency
//     invalidate/revalidate overrides on ANY org's bucket.
//   - **Org admin** — wallet holds one or more `OrgCap`s (`useOrgCaps`):
//     org fee + fee withdrawal, and invalidate / revalidate / cleanup on
//     the org's own buckets.
//
// Creating an org is PERMISSIONLESS — the create form shows for any
// connected wallet. The nav link in `Header` is gated on either capability;
// wallets with neither hitting `/admin` directly are redirected to `/earn`,
// but the create-org form remains reachable for them via the redirect-free
// loading state below only when they hold a cap (org creation for fresh
// wallets happens through this same screen once linked from docs).

import { useMemo, useState } from "react";
import { Navigate } from "react-router-dom";
import { useCurrentAccount } from "@mysten/dapp-kit";
import type { Transaction } from "@mysten/sui/transactions";
import { useSubmitTransaction } from "../tx/submit";

import { Header } from "../components/Header";
import { Toast } from "../components/Toast";
import { TokenManager } from "../components/TokenManager";
import { OrgManager } from "../components/OrgManager";
import { useBuckets } from "../api/useBuckets";
import { useAdminCap, useOrgCaps, type OwnedOrgCap } from "../api/useAdminCap";
import type { Bucket, Series } from "../api/client";
import {
  buildCreateTreasuryTx,
  buildInvalidateBucketTx,
  buildRevalidateBucketTx,
  buildSetFeeBpsTx,
  buildSetPauseTx,
  buildWithdrawTx,
} from "../tx/admin";
import {
  buildCreateOrgTx,
  buildOrgCleanupBucketTx,
  buildOrgInvalidateBucketTx,
  buildOrgRevalidateBucketTx,
  buildOrgWithdrawTx,
  buildSetOrgFeeTx,
} from "../tx/org";
import { PROTOCOL_CONFIG_ID, TREASURY_ID } from "../config";

function scaleRaw(raw: string, decimals: number | null): string {
  if (decimals === null) return raw;
  const v = Number(BigInt(raw)) / 10 ** decimals;
  return v.toLocaleString("en-US", { maximumFractionDigits: 6 });
}

/** Normalize object ids for comparison (0x-prefix + lowercase). */
function sameId(a: string, b: string): boolean {
  const norm = (s: string) => s.replace(/^0x/, "").replace(/^0+/, "").toLowerCase();
  return norm(a) === norm(b);
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
  const orgCaps = useOrgCaps(wallet);
  const buckets = useBuckets();
  const submitTx = useSubmitTransaction();

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

  // Wallet must be connected and hold either capability. While the cap
  // queries resolve we show a spinner rather than flashing a redirect.
  if (!wallet) return <Navigate to="/earn" replace />;
  if (adminCap.isLoading || orgCaps.isLoading) {
    return (
      <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
        <Header />
        <div className="app__wrap">
          <div className="admin-gate">checking admin access…</div>
        </div>
      </div>
    );
  }
  const isPlatformAdmin = (adminCap.data?.isAdmin ?? false) && !!adminCap.data?.adminCapId;
  const adminCapId = adminCap.data?.adminCapId ?? null;
  const myOrgCaps: OwnedOrgCap[] = orgCaps.data ?? [];
  if (!isPlatformAdmin && myOrgCaps.length === 0) {
    return <Navigate to="/earn" replace />;
  }

  /** The wallet's OrgCap for a series' org, if it holds one. */
  const capForOrg = (orgId: string): OwnedOrgCap | undefined =>
    myOrgCaps.find((c) => sameId(c.orgId, orgId));

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
      await submitTx(tx);
      flash(`✓ ${ok}`);
      buckets.refetch();
      orgCaps.refetch();
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      flash(`failed · ${message}`);
    } finally {
      setBusy(null);
    }
  };

  return (
    <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
      <Header />

      <div className="app__wrap">
        <div className="dash-hero">
          <div className="dash-hero__eyebrow">
            privileged ·{" "}
            {isPlatformAdmin
              ? "AdminCap holder"
              : `org admin (${myOrgCaps.length} cap${myOrgCaps.length === 1 ? "" : "s"})`}
          </div>
          <h1 className="dash-hero__title">Admin</h1>
          {adminCapId && (
            <div className="dash-hero__addr">
              cap {adminCapId.slice(0, 6)}…{adminCapId.slice(-4)}
            </div>
          )}
        </div>

        {/* ── Buckets ─────────────────────────────────────────────── */}
        <section className="admin-section">
          <div className="admin-section__head">
            <h2 className="admin-section__title">Call options &amp; buckets</h2>
            <div className="admin-section__sub">
              every verified bucket · invalidate freezes new writes
              (exercise/redeem unaffected) · org caps gate their own buckets;
              the AdminCap can override-gate any bucket
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
                <span>Asset · org</span>
                <span>Strike</span>
                <span>Expiry</span>
                <span>Written / Exercised</span>
                <span>Status</span>
                <span>Actions</span>
              </div>
              {flat.map(({ series, bucket, expired }) => {
                const key = bucket.bucket_id;
                const orgCap = capForOrg(series.org_id);
                const gateParams = () => ({
                  bucketId: bucket.bucket_id,
                  underlyingCoinType: series.asset_coin_type,
                  settlementCoinType: series.settlement_coin_type,
                  callCoinType: bucket.call_coin_type,
                  reason,
                });
                return (
                  <div className="admin-table__row" key={key}>
                    <span className="admin-cell__asset">
                      {series.asset_symbol}/{series.settlement_symbol}
                      {series.org_name && (
                        <span className="admin-tag admin-tag--mute"> {series.org_name}</span>
                      )}
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
                      {!expired && !bucket.invalidated && (orgCap || isPlatformAdmin) && (
                        <button
                          className="admin-btn admin-btn--danger"
                          disabled={busy !== null}
                          title={orgCap ? "Gate with your OrgCap" : "AdminCap override"}
                          onClick={() =>
                            run(
                              `inv-${key}`,
                              () =>
                                orgCap
                                  ? buildOrgInvalidateBucketTx({
                                      orgCapId: orgCap.capId,
                                      ...gateParams(),
                                    })
                                  : buildInvalidateBucketTx({
                                      adminCapId: adminCapId as string,
                                      ...gateParams(),
                                    }),
                              "bucket invalidated",
                            )
                          }
                        >
                          {busy === `inv-${key}` ? "…" : "Invalidate"}
                        </button>
                      )}
                      {!expired && bucket.invalidated && (orgCap || isPlatformAdmin) && (
                        <button
                          className="admin-btn"
                          disabled={busy !== null}
                          title={orgCap ? "Gate with your OrgCap" : "AdminCap override"}
                          onClick={() =>
                            run(
                              `rev-${key}`,
                              () =>
                                orgCap
                                  ? buildOrgRevalidateBucketTx({
                                      orgCapId: orgCap.capId,
                                      ...gateParams(),
                                    })
                                  : buildRevalidateBucketTx({
                                      adminCapId: adminCapId as string,
                                      ...gateParams(),
                                    }),
                              "bucket revalidated",
                            )
                          }
                        >
                          {busy === `rev-${key}` ? "…" : "Revalidate"}
                        </button>
                      )}
                      {expired && orgCap && (
                        <button
                          className="admin-btn"
                          disabled={busy !== null}
                          title="Only succeeds once the bucket's balances are fully drained. Returns the option coin's TreasuryCap to your wallet."
                          onClick={() =>
                            run(
                              `cln-${key}`,
                              () =>
                                buildOrgCleanupBucketTx({
                                  orgCapId: orgCap.capId,
                                  bucketId: bucket.bucket_id,
                                  underlyingCoinType: series.asset_coin_type,
                                  settlementCoinType: series.settlement_coin_type,
                                  callCoinType: bucket.call_coin_type,
                                  capRecipient: wallet,
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

        {/* ── Org admin (per owned OrgCap) ────────────────────────── */}
        {myOrgCaps.map((cap) => (
          <OrgAdminForms
            key={cap.capId}
            cap={cap}
            busy={busy}
            onRun={run}
          />
        ))}

        {/* ── Create an org (permissionless) ──────────────────────── */}
        <CreateOrgForm
          busy={busy}
          wallet={wallet}
          onSubmit={(build) => run("create-org", build, "org created — cap in your wallet")}
        />

        {/* ── Platform-admin-only sections ────────────────────────── */}
        {isPlatformAdmin && adminCapId && (
          <>
            <SetFeeForm
              busy={busy}
              onSubmit={(build) => run("set-fee", build, "protocol fee updated")}
              adminCapId={adminCapId}
            />
            <PauseForm
              busy={busy}
              onSubmit={(build, ok) => run("set-pause", build, ok)}
              adminCapId={adminCapId}
            />
            <TreasuryForms
              busy={busy}
              onWithdraw={(build) => run("withdraw", build, "treasury withdrawn")}
              onCreate={(build) => run("create-treasury", build, "treasury created")}
              adminCapId={adminCapId}
            />
            <OrgManager flash={flash} />
            <TokenManager flash={flash} />
          </>
        )}
      </div>

      {toast && <Toast message={toast} />}
    </div>
  );
}

// ── Org admin (fee + withdraw) ──────────────────────────────────────────

function OrgAdminForms({
  cap,
  busy,
  onRun,
}: {
  cap: OwnedOrgCap;
  busy: string | null;
  onRun: (key: string, build: () => Transaction, ok: string) => void;
}) {
  const [bps, setBps] = useState("0");
  const [coinType, setCoinType] = useState("");
  const [amount, setAmount] = useState("");
  const [recipient, setRecipient] = useState("");
  const withdrawReady = coinType && amount && recipient;

  return (
    <section className="admin-section">
      <div className="admin-section__head">
        <h2 className="admin-section__title">Your org</h2>
        <div className="admin-section__sub">
          org{" "}
          <code className="admin-cell__id">
            {cap.orgId.slice(0, 8)}…{cap.orgId.slice(-6)}
          </code>{" "}
          · fee in bps of gross premium (max 1000) · withdraw accrued org fees
        </div>
      </div>
      <div className="admin-grid">
        <Field label="Org fee (bps)">
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
        disabled={busy !== null}
        onClick={() =>
          onRun(
            `org-fee-${cap.capId}`,
            () =>
              buildSetOrgFeeTx({
                orgCapId: cap.capId,
                orgId: cap.orgId,
                newBps: BigInt(bps || "0"),
              }),
            "org fee updated",
          )
        }
      >
        {busy === `org-fee-${cap.capId}` ? "updating…" : "Set org fee"}
      </button>

      <div className="admin-grid" style={{ marginTop: 16 }}>
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
      <button
        className="admin-btn admin-btn--primary"
        disabled={!withdrawReady || busy !== null}
        onClick={() =>
          onRun(
            `org-withdraw-${cap.capId}`,
            () =>
              buildOrgWithdrawTx({
                orgCapId: cap.capId,
                orgId: cap.orgId,
                coinType,
                amountRaw: BigInt(amount || "0"),
                recipient,
              }),
            "org fees withdrawn",
          )
        }
      >
        {busy === `org-withdraw-${cap.capId}` ? "withdrawing…" : "Withdraw org fees"}
      </button>
    </section>
  );
}

// ── Create org (permissionless) ─────────────────────────────────────────

function CreateOrgForm({
  busy,
  wallet,
  onSubmit,
}: {
  busy: string | null;
  wallet: string;
  onSubmit: (build: () => Transaction) => void;
}) {
  const [name, setName] = useState("");
  const [bps, setBps] = useState("0");
  const ready = name.length > 0 && name.length <= 64;

  return (
    <section className="admin-section">
      <div className="admin-section__head">
        <h2 className="admin-section__title">Create an org</h2>
        <div className="admin-section__sub">
          permissionless — anyone can create an org and roll buckets under it
          (via a self-hosted option-scheduler). Buckets only appear in this app
          once the platform verifies the org.
        </div>
      </div>
      <div className="admin-grid">
        <Field label="Name (≤ 64 bytes)">
          <input
            className="admin-field__input"
            placeholder="Acme Options"
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
        </Field>
        <Field label="Org fee (bps, max 1000)">
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
        disabled={!ready || busy !== null}
        onClick={() =>
          onSubmit(() =>
            buildCreateOrgTx({
              name,
              feeBps: BigInt(bps || "0"),
              capRecipient: wallet,
            }),
          )
        }
      >
        {busy === "create-org" ? "creating…" : "Create org"}
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
          the protocol-level skim, taken on every write across ALL orgs ·
          basis points · max 1000 (10%). e.g. 50 = 0.5%. Org fees stack on top.
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

// ── Pause ──────────────────────────────────────────────────────────────

function PauseForm({
  busy,
  onSubmit,
  adminCapId,
}: {
  busy: string | null;
  onSubmit: (build: () => Transaction, ok: string) => void;
  adminCapId: string;
}) {
  const configMissing = !PROTOCOL_CONFIG_ID;

  return (
    <section className="admin-section">
      <div className="admin-section__head">
        <h2 className="admin-section__title">Emergency pause</h2>
        <div className="admin-section__sub">
          blocks NEW WRITES across all orgs' buckets — exercises, redeems, and
          burns are never blocked.
        </div>
      </div>
      <div className="admin-actions-row">
        <button
          className="admin-btn admin-btn--danger"
          disabled={configMissing || busy !== null}
          onClick={() =>
            onSubmit(
              () =>
                buildSetPauseTx({
                  adminCapId,
                  protocolConfigId: PROTOCOL_CONFIG_ID as string,
                  paused: true,
                }),
              "protocol paused (writes blocked)",
            )
          }
        >
          {busy === "set-pause" ? "…" : "Pause writes"}
        </button>
        <button
          className="admin-btn"
          disabled={configMissing || busy !== null}
          onClick={() =>
            onSubmit(
              () =>
                buildSetPauseTx({
                  adminCapId,
                  protocolConfigId: PROTOCOL_CONFIG_ID as string,
                  paused: false,
                }),
              "protocol unpaused",
            )
          }
        >
          {busy === "set-pause" ? "…" : "Unpause"}
        </button>
      </div>
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
