// Human ↔ raw-unit conversion. All amount math is bigint — floats never touch
// on-chain quantities.

/** "1.5" with 9 decimals → 1_500_000_000n. Throws on malformed input. */
export function parseUnits(input: string, decimals: number): bigint {
  const s = input.trim();
  if (!/^\d*(\.\d*)?$/.test(s) || s === "" || s === ".") {
    throw new Error(`invalid amount: "${input}"`);
  }
  const [whole, frac = ""] = s.split(".");
  if (frac.length > decimals) {
    throw new Error(`too many decimal places (max ${decimals})`);
  }
  return BigInt(whole || "0") * 10n ** BigInt(decimals) + BigInt(frac.padEnd(decimals, "0") || "0");
}

/** 1_500_000_000n with 9 decimals → "1.5" (trailing zeros trimmed). */
export function formatUnits(raw: bigint, decimals: number): string {
  const base = 10n ** BigInt(decimals);
  const whole = raw / base;
  const frac = (raw % base).toString().padStart(decimals, "0").replace(/0+$/, "");
  return frac ? `${whole}.${frac}` : whole.toString();
}

/**
 * The orderbook API is inconsistent about u64 encoding (SignedOrder amounts
 * are strings, RoutePlan amounts are JSON numbers) — accept both.
 */
export function toBigint(v: string | number): bigint {
  return typeof v === "number" ? BigInt(Math.trunc(v)) : BigInt(v);
}

/** Short 0x1234…abcd form for addresses/object ids. */
export function shortId(id: string): string {
  return id.length > 14 ? `${id.slice(0, 8)}…${id.slice(-6)}` : id;
}

const TYPE_RE = /^0x([0-9a-fA-F]{1,64})::([A-Za-z_][A-Za-z0-9_]*)::([A-Za-z_][A-Za-z0-9_]*)$/;

/**
 * "0x2::sui::SUI" → the canonical "0x" + 64-hex form the contracts and
 * deployments.json use. Returns null for anything that isn't a plain
 * (non-generic) struct type.
 */
export function canonicalizeType(s: string): string | null {
  const m = TYPE_RE.exec(s.trim());
  if (!m) return null;
  return `0x${m[1].toLowerCase().padStart(64, "0")}::${m[2]}::${m[3]}`;
}

/** Last path segment of a Move type ("0x…::usdc::USDC" → "USDC"). */
export function typeName(moveType: string): string {
  const seg = moveType.split("::").pop() ?? moveType;
  return seg.split("<")[0];
}
