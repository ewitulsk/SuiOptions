// Small shared building blocks: cards, tiles, pills, meters.

import type { ReactNode } from "react";

export function Card(props: {
  title: string;
  sub?: string;
  span?: "full" | "half" | "third";
  children: ReactNode;
  actions?: ReactNode;
}) {
  const span =
    props.span === "half" ? "dash-card--half" : props.span === "third" ? "dash-card--third" : "";
  return (
    <section className={`dash-card ${span}`}>
      <div className="dash-card__head">
        {props.title}
        {props.sub && <span className="dash-card__sub">{props.sub}</span>}
        {props.actions && <span style={{ marginLeft: "auto" }}>{props.actions}</span>}
      </div>
      {props.children}
    </section>
  );
}

export function Tile(props: { label: string; value: ReactNode; hint?: ReactNode }) {
  return (
    <div className="dash-tile">
      <div className="dash-tile__label">{props.label}</div>
      <div className="dash-tile__value">{props.value}</div>
      {props.hint != null && <div className="dash-tile__hint">{props.hint}</div>}
    </div>
  );
}

export type PillTone = "ok" | "warn" | "bad" | "muted";

export function Pill(props: { tone: PillTone; children: ReactNode; title?: string }) {
  const tone = props.tone === "muted" ? "" : `dash-pill--${props.tone}`;
  return (
    <span className={`dash-pill ${tone}`} title={props.title}>
      {props.children}
    </span>
  );
}

/**
 * Utilization meter: `value` is utilization of the SOFT limit (1.0 = at
 * soft), `hardAt` the hard limit expressed in the same scale (e.g.
 * hard/soft ratio). Green under 0.7, amber to the soft limit, red past.
 */
export function Meter(props: { label: string; value: number; detail?: string }) {
  const v = Number.isFinite(props.value) ? Math.max(0, props.value) : 0;
  const fillClass =
    v >= 1 ? "dash-meter__fill--bad" : v >= 0.7 ? "dash-meter__fill--warn" : "";
  return (
    <div className="dash-meter">
      <div className="dash-meter__row">
        <span>{props.label}</span>
        <b>
          {(v * 100).toFixed(0)}%{props.detail ? ` · ${props.detail}` : ""}
        </b>
      </div>
      <div className="dash-meter__track">
        <div
          className={`dash-meter__fill ${fillClass}`}
          style={{ width: `${Math.min(100, v * 100)}%` }}
        />
      </div>
    </div>
  );
}

export function Empty(props: { children: ReactNode }) {
  return <div className="dash-empty">{props.children}</div>;
}

export function ErrorNote(props: { error: unknown; what: string }) {
  const msg = props.error instanceof Error ? props.error.message : String(props.error);
  return (
    <div className="dash-empty">
      Failed to load {props.what}: <span className="neg">{msg}</span>
    </div>
  );
}
