// Canonical SIWS-session message serializer.
//
// This MUST stay byte-for-byte identical to `contracts/sources/message.move`.
// The contract rebuilds these exact bytes from on-chain/checked values and
// verifies the Solana signature against them — so any divergence here breaks
// sign-in entirely. Both sides are pinned against the same reference vectors
// in their respective test suites (`message.test.ts` / `session_tests.move`).
//
// Format (ASCII, single `\n` separators, no trailing newline):
//
//   siws-session-v2
//   domain: 0x<registry addr>
//   chain: sui:<network>
//   account: 0x<solana ed25519 pubkey>
//   session_key: 0x<temp sui addr>
//   generation: <u64>
//   nonce: 0x<32-byte nonce>
//   expires_at_ms: <u64>
//   limits: <type>=<per_tx>/<total>,... (or `none`)
//
// v2 adds the `limits:` line so the cap's per-coin-type spend limits are
// covered by the root signature.

import { normalizeStructTag, normalizeSuiAddress } from "@mysten/sui/utils";

/** A per-coin-type spend limit carried by the cap. */
export interface SpendLimit {
  /** Fully-qualified coin type (any 0x form; canonicalized internally). */
  coinType: string;
  /** Per-transaction max, in the coin's base units. */
  perTx: bigint;
  /** Max cumulative spend over the cap's life. */
  total: bigint;
}

export interface SessionMessageFields {
  /** Registry object address — the cross-contract replay domain. */
  domain: string;
  /** Chain tag, e.g. "testnet". Combined as `sui:<network>`. */
  network: string;
  /** Raw 32-byte Solana ed25519 public key. */
  solanaPk: Uint8Array;
  /** Ephemeral Sui address the cap is minted to. */
  sessionKey: string;
  generation: bigint | number;
  /** Random 32-byte nonce. */
  nonce: Uint8Array;
  expiresAtMs: bigint | number;
  limits: SpendLimit[];
}

export interface RevokeMessageFields {
  domain: string;
  network: string;
  solanaPk: Uint8Array;
  /** The user's Account object id. */
  accountId: string;
  nonce: Uint8Array;
  expiresAtMs: bigint | number;
}

const HEX = "0123456789abcdef";

/** Lowercase hex with a `0x` prefix — mirrors Move's `hex0x`. */
function hex0x(bytes: Uint8Array): string {
  let out = "0x";
  for (const b of bytes) out += HEX[b >> 4] + HEX[b & 0xf];
  return out;
}

/** A normalized Sui address is already `0x` + 64 lowercase hex chars, which is
 *  exactly `hex0x(address::to_bytes(addr))` on-chain. */
function addrHex(addr: string): string {
  return normalizeSuiAddress(addr);
}

function field(label: string, value: string): string {
  return `\n${label}${value}`;
}

/**
 * Canonical coin-type string: `0x` + 64-hex padded address + `::module::Name`
 * — the exact bytes `account::canonical_type_bytes<T>()` produces on-chain.
 */
export function canonicalCoinType(coinType: string): string {
  return normalizeStructTag(coinType);
}

/**
 * `<type>=<per_tx>/<total>` joined by `,`; `none` when empty. Mirrors Move's
 * `message::encode_limits` byte-for-byte (it is rendered into both the SIWS
 * and SIWE signed messages).
 */
export function encodeLimits(limits: SpendLimit[]): string {
  if (limits.length === 0) return "none";
  return limits
    .map(
      (l) =>
        `${canonicalCoinType(l.coinType)}=${BigInt(l.perTx).toString(10)}/${BigInt(l.total).toString(10)}`,
    )
    .join(",");
}

export function serializeSessionMessage(f: SessionMessageFields): Uint8Array {
  const msg =
    "siws-session-v2" +
    field("domain: ", addrHex(f.domain)) +
    field("chain: ", `sui:${f.network}`) +
    field("account: ", hex0x(f.solanaPk)) +
    field("session_key: ", addrHex(f.sessionKey)) +
    field("generation: ", BigInt(f.generation).toString(10)) +
    field("nonce: ", hex0x(f.nonce)) +
    field("expires_at_ms: ", BigInt(f.expiresAtMs).toString(10)) +
    field("limits: ", encodeLimits(f.limits));
  return new TextEncoder().encode(msg);
}

export function serializeRevokeMessage(f: RevokeMessageFields): Uint8Array {
  const msg =
    "siws-session-revoke-v1" +
    field("domain: ", addrHex(f.domain)) +
    field("chain: ", `sui:${f.network}`) +
    field("account: ", hex0x(f.solanaPk)) +
    field("account_id: ", addrHex(f.accountId)) +
    field("nonce: ", hex0x(f.nonce)) +
    field("expires_at_ms: ", BigInt(f.expiresAtMs).toString(10));
  return new TextEncoder().encode(msg);
}
