import { useState } from "react";
import type { ReactNode } from "react";

import { ApiError } from "../api/dakota";

export function Panel({ title, hint, children }: { title?: string; hint?: string; children: ReactNode }) {
  return (
    <div className="panel">
      {title && <h3>{title}</h3>}
      {hint && <p className="hint">{hint}</p>}
      {children}
    </div>
  );
}

/** Renders an error the way the service meant it to be read.
 *
 *  dakota-service relays Dakota's RFC 9457 `detail` verbatim because those
 *  messages name the actual problem; the request id is worth showing because
 *  it is the first thing Dakota support asks for. */
export function ErrorBox({ error }: { error: unknown }) {
  if (!error) return null;
  const msg = error instanceof Error ? error.message : String(error);
  const api = error instanceof ApiError ? error : null;
  return (
    <div className="error">
      <div>{msg}</div>
      {api?.fields?.length ? (
        <ul style={{ margin: "6px 0 0", paddingLeft: 18 }}>
          {api.fields.map((f, i) => (
            <li key={i}>
              {f.field ? <code>{f.field}</code> : null} {f.message}
            </li>
          ))}
        </ul>
      ) : null}
      {api?.dakotaRequestId ? (
        <div className="mono" style={{ marginTop: 6, opacity: 0.8 }}>
          dakota request id: {api.dakotaRequestId}
        </div>
      ) : null}
    </div>
  );
}

export function StatusPill({ status }: { status: string | null | undefined }) {
  if (!status) return <span className="pill">unknown</span>;
  const s = status.toLowerCase();
  const tone =
    s === "active" || s === "approved" || s === "completed" || s === "settled"
      ? "ok"
      : s === "rejected" || s === "failed" || s === "frozen"
        ? "err"
        : s === "pending" || s === "processing" || s === "not_started"
          ? "warn"
          : "";
  return <span className={`pill ${tone}`}>{status}</span>;
}

/** A value the user needs to hand to someone else — an invite link, a deposit
 *  address, a set of wire details. Copying is the whole point, so it is one
 *  click and confirms itself. */
export function CopyField({ label, value }: { label: string; value: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <label>
      <span>{label}</span>
      <div className="copy-row">
        <input readOnly value={value} onFocus={(e) => e.currentTarget.select()} />
        <button
          type="button"
          className="secondary"
          onClick={() => {
            navigator.clipboard?.writeText(value).then(
              () => {
                setCopied(true);
                setTimeout(() => setCopied(false), 1500);
              },
              () => {},
            );
          }}
        >
          {copied ? "copied" : "copy"}
        </button>
      </div>
    </label>
  );
}

export function Empty({ children }: { children: ReactNode }) {
  return <div className="empty">{children}</div>;
}

export function Table({ head, children }: { head: ReactNode; children: ReactNode }) {
  return (
    <div className="scroll-x">
      <table>
        <thead>{head}</thead>
        <tbody>{children}</tbody>
      </table>
    </div>
  );
}

/** Short form of a KSUID, which is 27 characters of noise in a table cell. */
export const shortId = (id: string | null | undefined) =>
  !id ? "—" : id.length <= 12 ? id : `${id.slice(0, 6)}…${id.slice(-4)}`;

export const fmtTime = (t: string | null | undefined) =>
  !t ? "—" : new Date(t).toLocaleString();
