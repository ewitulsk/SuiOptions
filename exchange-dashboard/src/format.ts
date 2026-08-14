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

const ATOM_RE = /^(0x)?([0-9a-fA-F]{1,64})::([A-Za-z_][A-Za-z0-9_]*)::([A-Za-z_][A-Za-z0-9_]*)$/;

/**
 * Canonical "0x" + 64-hex form the contracts and deployments.json use — at
 * EVERY nesting depth. Any-strike option coins are generic instantiations
 * (`0x…::option_coin::OptionCall<0x…::tbtc::TBTC,…>`), so the old
 * flat-only regex rejected exactly the markets this dashboard now needs.
 * Returns null when any struct atom is malformed; primitives (`u64`,
 * `vector`) pass through.
 */
export function canonicalizeType(s: string): string | null {
  let out = "";
  let atom = "";
  const flush = (): boolean => {
    if (!atom) return true;
    const trimmed = atom.trim();
    if (!trimmed) {
      atom = "";
      return true;
    }
    if (trimmed.includes("::")) {
      const m = ATOM_RE.exec(trimmed);
      if (!m) return false;
      out += `0x${m[2].toLowerCase().padStart(64, "0")}::${m[3]}::${m[4]}`;
    } else {
      out += trimmed; // primitive / vector keyword
    }
    atom = "";
    return true;
  };
  for (const c of s.trim()) {
    if (c === "<" || c === ">" || c === ",") {
      if (!flush()) return null;
      out += c;
    } else if (/\s/.test(c)) {
      if (!flush()) return null;
    } else {
      atom += c;
    }
  }
  if (!flush()) return null;
  return out || null;
}

/**
 * Display name of a Move type: the ROOT struct's name, generic args
 * stripped ("0x…::usdc::USDC" → "USDC";
 * "0x…::option_coin::OptionCall<…>" → "OptionCall"). The old
 * split("::").pop() walked into the generic args and produced garbage for
 * nested types.
 */
export function typeName(moveType: string): string {
  const root = moveType.split("<")[0];
  return root.split("::").pop() ?? moveType;
}
