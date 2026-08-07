// Display formatting helpers. Raw amounts are integer smallest-units;
// `decimals` scales them to human units.

export function fromRaw(raw: number | string | null | undefined, decimals: number): number | null {
  if (raw == null) return null;
  const n = typeof raw === "string" ? Number(raw) : raw;
  if (!Number.isFinite(n)) return null;
  return n / 10 ** decimals;
}

/** Human amount with thousands separators and sensible precision. */
export function fmtAmount(v: number | null | undefined, maxFrac = 2): string {
  if (v == null || !Number.isFinite(v)) return "—";
  const abs = Math.abs(v);
  const frac = abs !== 0 && abs < 1 ? Math.min(6, maxFrac + 2) : maxFrac;
  return v.toLocaleString("en-US", { maximumFractionDigits: frac });
}

export function fmtRaw(
  raw: number | string | null | undefined,
  decimals: number,
  maxFrac = 2,
): string {
  return fmtAmount(fromRaw(raw, decimals), maxFrac);
}

export function fmtPct(v: number | null | undefined, maxFrac = 1): string {
  if (v == null || !Number.isFinite(v)) return "—";
  return `${(v * 100).toLocaleString("en-US", { maximumFractionDigits: maxFrac })}%`;
}

/** Signed number with an explicit +. */
export function fmtSigned(v: number | null | undefined, maxFrac = 2): string {
  if (v == null || !Number.isFinite(v)) return "—";
  return `${v > 0 ? "+" : ""}${fmtAmount(v, maxFrac)}`;
}

export function shortId(id: string | null | undefined, chars = 6): string {
  if (!id) return "—";
  const clean = id.startsWith("0x") ? id : `0x${id}`;
  return `${clean.slice(0, 2 + chars)}…${clean.slice(-4)}`;
}

export function timeAgo(ms: number | null | undefined): string {
  if (!ms) return "—";
  const d = Date.now() - ms;
  if (d < 0) return "now";
  const s = Math.floor(d / 1000);
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 48) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}

export function fmtDate(ms: number | null | undefined): string {
  if (!ms) return "—";
  return new Date(ms).toLocaleString("en-US", {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function fmtExpiry(ms: number): string {
  const left = ms - Date.now();
  const date = new Date(ms).toLocaleDateString("en-US", { month: "short", day: "numeric" });
  if (left <= 0) return `${date} (expired)`;
  const days = left / 86_400_000;
  return days >= 2 ? `${date} (${Math.round(days)}d)` : `${date} (${Math.round(left / 3_600_000)}h)`;
}
