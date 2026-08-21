// Event-ingestor admin panel (lives on /admin).
//
// Config plane for the event→points pipeline feeding the leaderboard:
// track a Sui package (the service introspects its modules from chain),
// attach per-event points rules, and watch the ingestion streams. Every
// endpoint — reads included — requires an admin JWT obtained by signing a
// challenge with the connected wallet (see useAdminAuth); the wallet must
// be on the auth-service allowlist, so nothing renders before sign-in.

import { useState } from "react";
import { useCurrentAccount } from "@mysten/dapp-kit";
import { useQueryClient } from "@tanstack/react-query";

import { useAdminAuth } from "../api/useAdminAuth";
import {
  AuthExpiredError,
  canonicalEventType,
  createPackage,
  createRule,
  deletePackage,
  deleteRule,
  isCandidateEvent,
  patchRule,
  useIngestorPackages,
  useIngestorRules,
  useIngestorStatus,
  type BackfillState,
  type RecipientMode,
  type RuleDto,
  type StartMode,
  type StructDto,
  type TrackedPackageDto,
} from "../api/ingestorAdmin";

function shortId(s: string): string {
  return s.length > 24 ? `${s.slice(0, 10)}…${s.slice(-10)}` : s;
}

/** `datetime-local` value for an RFC3339 timestamp (local time). */
function toLocalInput(iso: string): string {
  const d = new Date(iso);
  if (isNaN(d.getTime())) return "";
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

export function EventIngestorManager({ flash }: { flash: (msg: string) => void }) {
  const account = useCurrentAccount();
  const auth = useAdminAuth();
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState<string | null>(null);
  // Package whose candidate events the rules section configures.
  const [selected, setSelected] = useState<string | null>(null);
  // Candidate event (canonical event_type) with its RuleForm open.
  const [editing, setEditing] = useState<string | null>(null);

  const getToken = auth.isSignedIn ? auth.getValidToken : null;
  const packagesQ = useIngestorPackages(getToken);
  const rulesQ = useIngestorRules(getToken);
  const statusQ = useIngestorStatus(getToken);

  const refetch = () =>
    Promise.all([
      queryClient.invalidateQueries({ queryKey: ["ingestor-packages"] }),
      queryClient.invalidateQueries({ queryKey: ["ingestor-rules"] }),
      queryClient.invalidateQueries({ queryKey: ["ingestor-status"] }),
      // Rule changes reshape the public board — drop every leaderboard read.
      queryClient.invalidateQueries({ queryKey: ["leaderboard"] }),
    ]);

  // Run an authenticated mutation, surfacing auth-expiry as a re-sign prompt.
  const runAuthed = async (key: string, fn: (token: string) => Promise<void>, ok: string) => {
    setBusy(key);
    try {
      const token = await auth.getValidToken();
      await fn(token);
      flash(`✓ ${ok}`);
      await refetch();
    } catch (e) {
      if (e instanceof AuthExpiredError) {
        auth.signOut();
        flash("session expired · sign in again");
      } else {
        flash(`failed · ${e instanceof Error ? e.message : String(e)}`);
      }
    } finally {
      setBusy(null);
    }
  };

  const onSignIn = async () => {
    try {
      await auth.signIn();
      flash("✓ signed in");
    } catch (e) {
      flash(`sign-in failed · ${e instanceof Error ? e.message : String(e)}`);
    }
  };

  const packages = packagesQ.data ?? [];
  const rules = rulesQ.data ?? [];
  const pkg = packages.find((p) => p.package_address === selected) ?? null;

  return (
    <section className="admin-section">
      <div className="admin-section__head">
        <h2 className="admin-section__title">Event ingestor</h2>
        <div className="admin-section__sub">
          leaderboard points pipeline · track packages, configure event→points rules,
          watch ingestion. Everything requires a signed-in admin wallet.
        </div>
      </div>

      {/* Sign-in row */}
      <div className="admin-actions-row" style={{ marginBottom: 16 }}>
        {auth.isSignedIn ? (
          <>
            <span className="admin-tag admin-tag--ok">
              signed in{auth.address ? ` · ${auth.address.slice(0, 6)}…${auth.address.slice(-4)}` : ""}
            </span>
            <button className="admin-btn" onClick={auth.signOut}>
              Sign out
            </button>
          </>
        ) : (
          <button
            className="admin-btn admin-btn--primary"
            disabled={!account || auth.busy}
            onClick={onSignIn}
            title={account ? "Sign a challenge to manage the ingestor" : "Connect a wallet first"}
          >
            {auth.busy ? "signing…" : "Sign in to manage the ingestor"}
          </button>
        )}
      </div>

      {!auth.isSignedIn && (
        <div className="admin-empty">sign in to view tracked packages, rules, and status.</div>
      )}

      {auth.isSignedIn && (
        <>
          {/* ── Tracked packages ─────────────────────────────────── */}
          {packagesQ.isLoading && <div className="admin-empty">loading packages…</div>}
          {packagesQ.error && (
            <div className="admin-empty">
              failed to load packages · {packagesQ.error.message}
            </div>
          )}
          {packagesQ.data && packages.length === 0 && (
            <div className="admin-empty">no packages tracked yet — add one below.</div>
          )}

          {packages.length > 0 && (
            <div className="admin-table">
              <div className="admin-table__head admin-table__row">
                <span>Package</span>
                <span>Label</span>
                <span>Modules</span>
                <span>Event types</span>
                <span>Actions</span>
              </div>
              {packages.map((p) => {
                const modules = p.modules.modules;
                const eventCount = modules.reduce(
                  (n, m) => n + m.structs.filter(isCandidateEvent).length,
                  0,
                );
                return (
                  <div className="admin-table__row" key={p.package_address}>
                    <span>
                      <code className="admin-cell__id">{shortId(p.package_address)}</code>
                    </span>
                    <span>{p.label}</span>
                    <span>{modules.length}</span>
                    <span>{eventCount}</span>
                    <span className="admin-cell__actions">
                      <button
                        className="admin-btn"
                        disabled={busy !== null}
                        onClick={() => {
                          setSelected(p.package_address);
                          setEditing(null);
                        }}
                      >
                        {selected === p.package_address ? "Configuring" : "Configure"}
                      </button>
                      <button
                        className="admin-btn admin-btn--danger"
                        disabled={busy !== null}
                        onClick={() => {
                          if (
                            !window.confirm(
                              `Remove ${p.label || shortId(p.package_address)}?\n\n` +
                                "All of its rules and ingestion cursors are deleted with it.",
                            )
                          ) {
                            return;
                          }
                          if (selected === p.package_address) setSelected(null);
                          runAuthed(
                            `del-pkg-${p.package_address}`,
                            (token) => deletePackage(token, p.package_address),
                            `removed ${p.label || shortId(p.package_address)}`,
                          );
                        }}
                      >
                        {busy === `del-pkg-${p.package_address}` ? "…" : "Remove"}
                      </button>
                    </span>
                  </div>
                );
              })}
            </div>
          )}

          <AddPackageForm
            busy={busy === "add-pkg"}
            onAdd={(input) =>
              runAuthed(
                "add-pkg",
                async (token) => {
                  const created = await createPackage(token, input);
                  setSelected(created.package_address);
                },
                `tracking ${input.label || shortId(input.package_address)}`,
              )
            }
          />

          {/* ── Points rules ─────────────────────────────────────── */}
          <div style={{ marginTop: 24 }}>
            <div className="admin-section__sub" style={{ marginBottom: 8 }}>
              Points rules
              {pkg ? (
                <>
                  {" "}
                  · <code>{pkg.label || shortId(pkg.package_address)}</code>
                </>
              ) : null}
            </div>
            {rulesQ.error && (
              <div className="admin-empty">failed to load rules · {rulesQ.error.message}</div>
            )}
            {!pkg ? (
              <div className="admin-empty">
                pick a tracked package (Configure) to see its candidate events.
              </div>
            ) : (
              <RulesTable
                pkg={pkg}
                rules={rules}
                busy={busy}
                editing={editing}
                setEditing={setEditing}
                runAuthed={runAuthed}
              />
            )}
          </div>

          {/* ── Ingestion status ─────────────────────────────────── */}
          <IngestionStatus
            statusError={statusQ.error}
            statusLoading={statusQ.isLoading}
            modules={statusQ.data?.modules ?? []}
            ruleStatuses={statusQ.data?.rules ?? []}
            rules={rules}
          />
        </>
      )}
    </section>
  );
}

function AddPackageForm({
  busy,
  onAdd,
}: {
  busy: boolean;
  onAdd: (input: { package_address: string; label?: string }) => void;
}) {
  const [packageAddress, setPackageAddress] = useState("");
  const [label, setLabel] = useState("");

  const ready = /^0x[0-9a-fA-F]{1,64}$/.test(packageAddress);

  return (
    <div style={{ marginTop: 20 }}>
      <div className="admin-section__sub" style={{ marginBottom: 8 }}>
        Track a package
      </div>
      <div className="admin-grid">
        <Field label="Package address">
          <input
            className="admin-field__input"
            placeholder="0x…"
            value={packageAddress}
            onChange={(e) => setPackageAddress(e.target.value.trim())}
          />
        </Field>
        <Field label="Label (optional)">
          <input
            className="admin-field__input"
            placeholder="exchange"
            value={label}
            onChange={(e) => setLabel(e.target.value)}
          />
        </Field>
      </div>
      <button
        className="admin-btn admin-btn--primary"
        disabled={!ready || busy}
        onClick={() =>
          onAdd({ package_address: packageAddress, ...(label ? { label } : {}) })
        }
      >
        {busy ? "introspecting…" : "Track package"}
      </button>
    </div>
  );
}

function RulesTable({
  pkg,
  rules,
  busy,
  editing,
  setEditing,
  runAuthed,
}: {
  pkg: TrackedPackageDto;
  rules: RuleDto[];
  busy: string | null;
  editing: string | null;
  setEditing: (e: string | null) => void;
  runAuthed: (key: string, fn: (token: string) => Promise<void>, ok: string) => Promise<void>;
}) {
  const candidates = pkg.modules.modules.flatMap((m) =>
    m.structs.filter(isCandidateEvent).map((s) => ({ module: m.name, struct: s })),
  );

  if (candidates.length === 0) {
    return <div className="admin-empty">no candidate event structs in this package.</div>;
  }

  return (
    <div className="admin-table">
      <div className="admin-table__head admin-table__row">
        <span>Event</span>
        <span>Rule</span>
        <span>Backfill</span>
        <span>Actions</span>
      </div>
      {candidates.map((c) => {
        const eventType = canonicalEventType(pkg.package_address, c.module, c.struct.name);
        const rule = rules.find((r) => r.event_type === eventType) ?? null;
        const open = editing === eventType;
        return (
          <div key={eventType}>
            <div className="admin-table__row">
              <span>
                <code className="admin-cell__id">
                  {c.module}::{c.struct.name}
                </code>
              </span>
              <span>
                {rule ? (
                  <>
                    <span className={"admin-tag " + (rule.enabled ? "admin-tag--ok" : "admin-tag--mute")}>
                      {rule.enabled ? "enabled" : "disabled"}
                    </span>{" "}
                    {rule.points} pts →{" "}
                    {rule.recipient_mode === "sender" ? "sender" : `field \`${rule.recipient_field}\``}
                  </>
                ) : (
                  <span className="admin-tag admin-tag--mute">no rule</span>
                )}
              </span>
              <span>
                {rule && rule.start_mode === "timestamp" ? (
                  <BackfillTag state={rule.backfill_state} />
                ) : (
                  <span className="admin-cell__dim">—</span>
                )}
              </span>
              <span className="admin-cell__actions">
                <button
                  className="admin-btn"
                  disabled={busy !== null}
                  onClick={() => setEditing(open ? null : eventType)}
                >
                  {open ? "Close" : rule ? "Edit" : "Add rule"}
                </button>
                {rule && (
                  <button
                    className="admin-btn admin-btn--danger"
                    disabled={busy !== null}
                    onClick={() => {
                      if (
                        !window.confirm(
                          `Delete the rule for ${c.module}::${c.struct.name}?\n\n` +
                            "Already-awarded points are kept; new events stop earning.",
                        )
                      ) {
                        return;
                      }
                      runAuthed(
                        `del-rule-${rule.id}`,
                        (token) => deleteRule(token, rule.id),
                        `deleted rule for ${c.struct.name}`,
                      );
                    }}
                  >
                    {busy === `del-rule-${rule.id}` ? "…" : "Delete"}
                  </button>
                )}
              </span>
            </div>
            {open && (
              <RuleForm
                struct={c.struct}
                rule={rule}
                busy={busy === `save-${eventType}`}
                onSubmit={(form) => {
                  const recipient =
                    form.recipient_mode === "field"
                      ? { recipient_field: form.recipient_field }
                      : {};
                  const start =
                    form.start_mode === "timestamp" ? { start_at: form.start_at } : {};
                  if (rule) {
                    runAuthed(
                      `save-${eventType}`,
                      async (token) => {
                        await patchRule(token, rule.id, {
                          label: form.label,
                          points: form.points,
                          enabled: form.enabled,
                          recipient_mode: form.recipient_mode,
                          start_mode: form.start_mode,
                          ...recipient,
                          ...start,
                        });
                        setEditing(null);
                      },
                      `updated rule for ${c.struct.name}`,
                    );
                  } else {
                    runAuthed(
                      `save-${eventType}`,
                      async (token) => {
                        await createRule(token, {
                          package_address: pkg.package_address,
                          module_name: c.module,
                          event_type: eventType,
                          label: form.label,
                          points: form.points,
                          recipient_mode: form.recipient_mode,
                          start_mode: form.start_mode,
                          enabled: form.enabled,
                          ...recipient,
                          ...start,
                        });
                        setEditing(null);
                      },
                      `added rule for ${c.struct.name}`,
                    );
                  }
                }}
              />
            )}
          </div>
        );
      })}
    </div>
  );
}

type RuleFormValues = {
  label: string;
  points: number;
  recipient_mode: RecipientMode;
  recipient_field: string;
  start_mode: StartMode;
  /** RFC3339, only meaningful when start_mode === "timestamp". */
  start_at: string;
  enabled: boolean;
};

function RuleForm({
  struct,
  rule,
  busy,
  onSubmit,
}: {
  struct: StructDto;
  rule: RuleDto | null;
  busy: boolean;
  onSubmit: (form: RuleFormValues) => void;
}) {
  const [label, setLabel] = useState(rule?.label ?? "");
  const [points, setPoints] = useState(rule ? String(rule.points) : "10");
  const [recipientMode, setRecipientMode] = useState<RecipientMode>(
    rule?.recipient_mode ?? "sender",
  );
  const [recipientField, setRecipientField] = useState(rule?.recipient_field ?? "");
  const [startMode, setStartMode] = useState<StartMode>(rule?.start_mode ?? "tip");
  const [startAt, setStartAt] = useState(rule?.start_at ? toLocalInput(rule.start_at) : "");
  const [enabled, setEnabled] = useState(rule?.enabled ?? true);

  // Address-typed fields first — they're the plausible recipients.
  const fields = [...struct.fields].sort(
    (a, b) => Number(b.repr === "address") - Number(a.repr === "address"),
  );

  const ready =
    label !== "" &&
    points !== "" &&
    Number.isFinite(Number(points)) &&
    (recipientMode !== "field" || recipientField !== "") &&
    (startMode !== "timestamp" || startAt !== "");

  return (
    <div style={{ padding: "10px 0 16px" }}>
      <div className="admin-grid">
        <Field label="Label">
          <input
            className="admin-field__input"
            placeholder="Fill an order"
            value={label}
            onChange={(e) => setLabel(e.target.value)}
          />
        </Field>
        <Field label="Points per event">
          <input
            className="admin-field__input"
            type="number"
            value={points}
            onChange={(e) => setPoints(e.target.value)}
          />
        </Field>
        <Field label="Recipient">
          <select
            className="admin-field__input"
            value={recipientMode}
            onChange={(e) => setRecipientMode(e.target.value as RecipientMode)}
          >
            <option value="sender">Transaction sender</option>
            <option value="field">Event field</option>
          </select>
        </Field>
        {recipientMode === "field" && (
          <Field label="Recipient field">
            <select
              className="admin-field__input"
              value={recipientField}
              onChange={(e) => setRecipientField(e.target.value)}
            >
              <option value="">pick a field…</option>
              {fields.map((f) => (
                <option key={f.name} value={f.name}>
                  {f.name} · {f.repr}
                </option>
              ))}
            </select>
          </Field>
        )}
        <Field label="Start">
          <div className="admin-actions-row" style={{ alignItems: "center" }}>
            <label style={{ display: "inline-flex", alignItems: "center", gap: 5 }}>
              <input
                type="radio"
                name={`start-${struct.name}`}
                checked={startMode === "tip"}
                onChange={() => setStartMode("tip")}
              />
              From now
            </label>
            <label style={{ display: "inline-flex", alignItems: "center", gap: 5 }}>
              <input
                type="radio"
                name={`start-${struct.name}`}
                checked={startMode === "timestamp"}
                onChange={() => setStartMode("timestamp")}
              />
              Backfill from timestamp
            </label>
          </div>
        </Field>
        {startMode === "timestamp" && (
          <Field label="Backfill from">
            <input
              className="admin-field__input"
              type="datetime-local"
              value={startAt}
              onChange={(e) => setStartAt(e.target.value)}
            />
          </Field>
        )}
        <Field label="Enabled">
          <input
            type="checkbox"
            checked={enabled}
            onChange={(e) => setEnabled(e.target.checked)}
          />
        </Field>
      </div>
      <button
        className="admin-btn admin-btn--primary"
        disabled={!ready || busy}
        onClick={() =>
          onSubmit({
            label,
            points: Number(points),
            recipient_mode: recipientMode,
            recipient_field: recipientField,
            start_mode: startMode,
            start_at: startAt ? new Date(startAt).toISOString() : "",
            enabled,
          })
        }
      >
        {busy ? "saving…" : rule ? "Save rule" : "Create rule"}
      </button>
    </div>
  );
}

function BackfillTag({ state }: { state: BackfillState }) {
  const cls =
    state === "done"
      ? "admin-tag--ok"
      : state === "exhausted"
        ? "admin-tag--danger"
        : "admin-tag--mute";
  return <span className={`admin-tag ${cls}`}>{state}</span>;
}

function IngestionStatus({
  statusLoading,
  statusError,
  modules,
  ruleStatuses,
  rules,
}: {
  statusLoading: boolean;
  statusError: Error | null;
  modules: {
    package_address: string;
    module: string;
    cursor_updated_at: string;
    last_event_ms: number | null;
    lag_ms: number | null;
  }[];
  ruleStatuses: {
    rule_id: number;
    backfill_state: BackfillState;
    delivered: number;
    last_delivery_at: string | null;
  }[];
  rules: RuleDto[];
}) {
  return (
    <div style={{ marginTop: 24 }}>
      <div className="admin-section__sub" style={{ marginBottom: 8 }}>
        Ingestion status
      </div>
      {statusLoading && <div className="admin-empty">loading status…</div>}
      {statusError && (
        <div className="admin-empty">failed to load status · {statusError.message}</div>
      )}
      {!statusLoading && !statusError && modules.length === 0 && (
        <div className="admin-empty">no module streams yet — enable a rule to start one.</div>
      )}
      {modules.length > 0 && (
        <div className="admin-table">
          <div className="admin-table__head admin-table__row">
            <span>Stream</span>
            <span>Last event</span>
            <span>Freshness</span>
          </div>
          {modules.map((m) => (
            <div className="admin-table__row" key={`${m.package_address}::${m.module}`}>
              <span>
                <code className="admin-cell__id">
                  {shortId(m.package_address)}::{m.module}
                </code>
              </span>
              <span>
                {m.last_event_ms !== null
                  ? new Date(m.last_event_ms).toLocaleString()
                  : "—"}
              </span>
              <span>
                <FreshnessTag lagMs={m.lag_ms} />
              </span>
            </div>
          ))}
        </div>
      )}
      {ruleStatuses.length > 0 && (
        <div className="admin-table" style={{ marginTop: 12 }}>
          <div className="admin-table__head admin-table__row">
            <span>Rule</span>
            <span>Backfill</span>
            <span>Delivered</span>
            <span>Last delivery</span>
          </div>
          {ruleStatuses.map((r) => {
            const rule = rules.find((x) => x.id === r.rule_id);
            return (
              <div className="admin-table__row" key={r.rule_id}>
                <span>{rule ? rule.label : `rule ${r.rule_id}`}</span>
                <span>
                  <BackfillTag state={r.backfill_state} />
                </span>
                <span>{r.delivered.toLocaleString("en-US")}</span>
                <span>
                  {r.last_delivery_at ? new Date(r.last_delivery_at).toLocaleString() : "—"}
                </span>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

function FreshnessTag({ lagMs }: { lagMs: number | null }) {
  if (lagMs === null) return <span className="admin-tag admin-tag--mute">no events</span>;
  if (lagMs < 60_000) return <span className="admin-tag admin-tag--ok">fresh</span>;
  const mins = Math.round(lagMs / 60_000);
  const cls = lagMs < 600_000 ? "admin-tag--mute" : "admin-tag--danger";
  return <span className={`admin-tag ${cls}`}>{mins}m behind</span>;
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="admin-field">
      <label className="admin-field__label">{label}</label>
      {children}
    </div>
  );
}
