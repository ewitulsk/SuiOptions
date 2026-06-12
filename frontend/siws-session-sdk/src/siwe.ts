// EIP-4361 (Sign-In With Ethereum) message builder + helpers.
//
// MUST stay byte-for-byte identical to `contracts/sources/siwe.move`. The
// contract reconstructs this exact message, applies the EIP-191 personal_sign
// prefix, and `ecrecover`s the secp256k1 signature to an Ethereum address.
// Pinned against the same reference vector in `siwe.test.ts` /
// `siwe_tests.move`.

import { keccak_256 } from "@noble/hashes/sha3.js";
import { normalizeSuiAddress } from "@mysten/sui/utils";

import { encodeLimits, type SpendLimit } from "./message.js";

// dApp display fields — MUST match siwe.move.
const DOMAIN = "siws-session.demo";
const URI = "https://siws-session.demo";
const SIGNIN_STATEMENT = "Authorize a Sui session key.";
const REVOKE_STATEMENT = "Revoke all Sui session keys.";

const HEXCHARS = "0123456789abcdef";

function bytesToHex(b: Uint8Array): string {
  let s = "";
  for (const x of b) s += HEXCHARS[x >> 4] + HEXCHARS[x & 0xf];
  return s;
}

function hexToBytes(hex: string): Uint8Array {
  const h = hex.startsWith("0x") ? hex.slice(2) : hex;
  const out = new Uint8Array(h.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(h.slice(i * 2, i * 2 + 2), 16);
  return out;
}

/** EIP-55 mixed-case checksum of a 20-byte address (no `0x`). */
function eip55(addr20: Uint8Array): string {
  const lower = bytesToHex(addr20);
  const h = keccak_256(new TextEncoder().encode(lower));
  let out = "";
  for (let i = 0; i < 40; i++) {
    const c = lower[i];
    const nibble = (h[i >> 1] >> (i % 2 === 0 ? 4 : 0)) & 0xf;
    out += /[a-f]/.test(c) && nibble >= 8 ? c.toUpperCase() : c;
  }
  return out;
}

export interface SiweSessionFields {
  /** Registry object address — the cross-contract replay domain (Resources). */
  registryDomain: string;
  /** 20-byte Ethereum address. */
  ethAddress: Uint8Array;
  /** Ephemeral Sui address the cap is minted to. */
  sessionKey: string;
  generation: bigint | number;
  nonce: Uint8Array;
  expiresAtMs: bigint | number;
  chainId: number;
  /** ISO-8601 timestamp string (verbatim). */
  issuedAt: string;
  /** Per-coin-type spend limits — rendered as a final `limits` resource. */
  limits: SpendLimit[];
}

export interface SiweRevokeFields {
  registryDomain: string;
  ethAddress: Uint8Array;
  /** The user's Account object id. */
  accountId: string;
  generation: bigint | number;
  nonce: Uint8Array;
  expiresAtMs: bigint | number;
  chainId: number;
  issuedAt: string;
}

function build(
  statement: string,
  registryDomain: string,
  ethAddress: Uint8Array,
  boundAddr: string,
  generation: bigint | number,
  nonce: Uint8Array,
  expiresAtMs: bigint | number,
  chainId: number,
  issuedAt: string,
): string {
  return (
    `${DOMAIN} wants you to sign in with your Ethereum account:\n` +
    `0x${eip55(ethAddress)}\n\n` +
    `${statement}\n\n` +
    `URI: ${URI}\n` +
    `Version: 1\n` +
    `Chain ID: ${chainId}\n` +
    `Nonce: ${bytesToHex(nonce)}\n` +
    `Issued At: ${issuedAt}\n` +
    `Resources:\n` +
    `- siws-session://sui-registry/${normalizeSuiAddress(registryDomain)}\n` +
    `- siws-session://session-key/${normalizeSuiAddress(boundAddr)}\n` +
    `- siws-session://generation/${BigInt(generation).toString()}\n` +
    `- siws-session://expires/${BigInt(expiresAtMs).toString()}`
  );
}

export function buildSiweSessionMessage(f: SiweSessionFields): string {
  return (
    build(
      SIGNIN_STATEMENT,
      f.registryDomain,
      f.ethAddress,
      f.sessionKey,
      f.generation,
      f.nonce,
      f.expiresAtMs,
      f.chainId,
      f.issuedAt,
    ) + `\n- siws-session://limits/${encodeLimits(f.limits)}`
  );
}

export function buildSiweRevokeMessage(f: SiweRevokeFields): string {
  return build(
    REVOKE_STATEMENT,
    f.registryDomain,
    f.ethAddress,
    f.accountId,
    f.generation,
    f.nonce,
    f.expiresAtMs,
    f.chainId,
    f.issuedAt,
  );
}

/** "0x…" address string → 20 raw bytes. */
export function ethAddressToBytes(addr: string): Uint8Array {
  const bytes = hexToBytes(addr.toLowerCase());
  if (bytes.length !== 20) throw new Error(`expected 20-byte eth address, got ${bytes.length}`);
  return bytes;
}

/**
 * Normalize a MetaMask `personal_sign` result (0x + 65 bytes, v ∈ {27,28} or
 * {0,1}) to the 65-byte `r||s||v` with v ∈ {0,1} that Sui's ecrecover expects.
 */
export function ethSignatureToSui(sigHex: string): Uint8Array {
  const bytes = hexToBytes(sigHex);
  if (bytes.length !== 65) throw new Error(`expected 65-byte eth signature, got ${bytes.length}`);
  const out = bytes.slice();
  if (out[64] >= 27) out[64] -= 27;
  return out;
}
