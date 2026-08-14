// Any-strike bucket creation (SO-395).
//
// `bucket::create_bucket_any_strike<U, S, D0..D9>` registers the bucket's
// option-coin currency at runtime (sui::coin_registry — no publish) and
// returns the bucket by value; `share_bucket` is the transaction's
// mandatory terminal command. The ten `D` type args are byte markers
// spelling the bucket's economics — this file mirrors the on-chain
// encoding in `contracts/core/sources/option_coin.move` and the Rust
// builder in `rust-backend/crates/sui-tx/src/tx/option_coin.rs`:
//
//   bytes 0..4   expiry, minutes since epoch (u32, big-endian)
//   bytes 4..9   strike significand (u40, big-endian)
//   byte  9      strike exponent: real strike = sig / 10^exp
//
// Markers `B00..B7F` live in module `enc0`, `B80..BFF` in `enc1`.

import { Transaction } from "@mysten/sui/transactions";
import { SUI_CLOCK_OBJECT_ID } from "@mysten/sui/utils";

import { BUCKET_REGISTRY_ID, ENV, PACKAGE_ID, WHITELIST_ID } from "../config";

/** Shared `sui::coin_registry::CoinRegistry` system object. */
const COIN_REGISTRY_ID = "0xc";

/** Largest significand the 5-byte encoding field carries (2^40 − 1). */
const MAX_SIG = 0xff_ffff_ffffn;

/** Canonical (significand, exponent): trailing zeros stripped — mirrors
 * `option_coin::normalize_strike`. Returns null for zero / over-precise
 * strikes (more than 13 significant digits). */
export function normalizeStrike(
  strikeRaw: bigint,
  strikeScale: number,
): { sig: bigint; exp: number } | null {
  if (strikeRaw <= 0n) return null;
  let sig = strikeRaw;
  let exp = strikeScale;
  while (sig % 10n === 0n && exp > 0) {
    sig /= 10n;
    exp -= 1;
  }
  if (sig > MAX_SIG) return null;
  return { sig, exp };
}

/** The ten byte-marker type names for a normalized spec. */
export function markerTypes(pkg: string, minutes: number, sig: bigint, exp: number): string[] {
  const bytes: number[] = [];
  for (let i = 3; i >= 0; i--) bytes.push((minutes >>> (8 * i)) & 0xff);
  for (let i = 4n; i >= 0n; i--) bytes.push(Number((sig >> (8n * i)) & 0xffn));
  bytes.push(exp & 0xff);
  return bytes.map((b) => {
    const module = b < 0x80 ? "enc0" : "enc1";
    return `${pkg}::${module}::B${b.toString(16).toUpperCase().padStart(2, "0")}`;
  });
}

/** `OptionCall<U, S, D0..D9>` (or the put twin) for a normalized spec. */
export function optionCoinTypeFor(
  pkg: string,
  isPut: boolean,
  underlying: string,
  settlement: string,
  minutes: number,
  sig: bigint,
  exp: number,
): string {
  const root = isPut ? "OptionPut" : "OptionCall";
  const args = [underlying, settlement, ...markerTypes(pkg, minutes, sig, exp)];
  return `${pkg}::option_coin::${root}<${args.join(",")}>`;
}

/**
 * Exact display-strike → raw (strike_raw, strike_scale) conversion.
 *
 * The bucket's ratio is settlement smallest-units per underlying
 * smallest-unit: `display × 10^(settleDec − underDec)`. Parsing the user's
 * decimal string digit-exactly (no float) keeps 13-digit strikes lossless.
 * Returns null on unparseable input.
 */
export function strikeDisplayToRaw(
  display: string,
  underDec: number,
  settleDec: number,
): { strikeRaw: bigint; strikeScale: number } | null {
  const m = display.trim().match(/^(\d+)(?:\.(\d+))?$/);
  if (!m) return null;
  const intPart = m[1];
  const frac = (m[2] ?? "").replace(/0+$/, "");
  const digits = `${intPart}${frac}`.replace(/^0+(?=\d)/, "");
  if (!/[1-9]/.test(digits)) return null;
  let value = BigInt(digits);
  // display = value × 10^(−frac.length); ratio = display × 10^(settle − under)
  let scale = frac.length - (settleDec - underDec);
  if (scale < 0) {
    value *= 10n ** BigInt(-scale);
    scale = 0;
  }
  if (scale > 38) return null;
  return { strikeRaw: value, strikeScale: scale };
}

export type CreateBucketParams = {
  /** `series.asset_coin_type`. */
  underlyingCoinType: string;
  /** `series.settlement_coin_type`. */
  settlementCoinType: string;
  expiryMs: number;
  strikeRaw: bigint;
  strikeScale: number;
  /** Option-coin display decimals — pass the underlying's. */
  coinDecimals: number;
  isPut: boolean;
};

/**
 * The sponsored create-bucket PTB (gas-station template `create_bucket` /
 * `put_create_bucket`): create at the arbitrary strike, then the mandatory
 * terminal `share_*`. Ingress-gated on-chain like writes.
 */
export function buildCreateBucketTx(p: CreateBucketParams): Transaction {
  if (!PACKAGE_ID || !WHITELIST_ID || !BUCKET_REGISTRY_ID) {
    throw new Error(
      `Missing packageId/whitelistId/bucketRegistryId for VITE_ENVIRONMENT="${ENV}" — this deployment predates any-strike creation`,
    );
  }
  if (p.expiryMs % 60_000 !== 0) {
    throw new Error("bucket expiries must be minute-aligned");
  }
  const norm = normalizeStrike(p.strikeRaw, p.strikeScale);
  if (!norm) {
    throw new Error("strike has too many significant digits (max 13)");
  }
  const minutes = p.expiryMs / 60_000;
  const module = p.isPut ? "put_bucket" : "bucket";
  const createFn = p.isPut ? "create_put_bucket_any_strike" : "create_bucket_any_strike";
  const shareFn = p.isPut ? "share_put_bucket" : "share_bucket";
  const coinType = optionCoinTypeFor(
    PACKAGE_ID,
    p.isPut,
    p.underlyingCoinType,
    p.settlementCoinType,
    minutes,
    norm.sig,
    norm.exp,
  );

  const tx = new Transaction();
  const bucket = tx.moveCall({
    target: `${PACKAGE_ID}::${module}::${createFn}`,
    typeArguments: [
      p.underlyingCoinType,
      p.settlementCoinType,
      ...markerTypes(PACKAGE_ID, minutes, norm.sig, norm.exp),
    ],
    arguments: [
      tx.object(BUCKET_REGISTRY_ID),
      tx.object(COIN_REGISTRY_ID),
      tx.object(WHITELIST_ID),
      tx.pure.u64(BigInt(p.expiryMs)),
      tx.pure.u128(p.strikeRaw),
      tx.pure.u8(p.strikeScale),
      tx.pure.u8(p.coinDecimals),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
  tx.moveCall({
    target: `${PACKAGE_ID}::${module}::${shareFn}`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, coinType],
    arguments: [bucket],
  });
  return tx;
}
