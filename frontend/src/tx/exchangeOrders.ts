// Consensus-critical mirror of `exchange::order` (SO-416).
//
// Order hashing must match contracts/exchange/sources/order.move and
// rust-backend/crates/exchange-signing/src/lib.rs byte-for-byte:
//
//   order_digest = blake2b256(
//     b"SUI_HYBRID_EXCHANGE_ORDER" ‖ [1u8] ‖ bcs(registry_id) ‖ bcs(Order)
//   )
//
// The maker's wallet then signs the 32-byte digest as a Sui PERSONAL MESSAGE
// (wallet-standard `signPersonalMessage` over the digest bytes) — the chain
// and the orderbook both verify over `blake2b256(intent ‖ bcs(digest))`.
// Soft cancels sign `b"SUI_HYBRID_EXCHANGE_CANCEL" ‖ digest` the same way
// (orderbook handlers.rs `cancel_order`).
//
// BCS field order mirrors exchange-types/src/order.rs — never reorder.
// Verified against exchange-signing/fixtures/conformance.json.

import { blake2b } from "@noble/hashes/blake2.js";
import { bcs } from "@mysten/sui/bcs";
import { fromBase64, normalizeSuiAddress } from "@mysten/sui/utils";

export const ORDER_DOMAIN_TAG = "SUI_HYBRID_EXCHANGE_ORDER";
export const ORDER_DOMAIN_VERSION = 1;
export const CANCEL_DOMAIN_TAG = "SUI_HYBRID_EXCHANGE_CANCEL";

export const ZERO_ADDRESS = normalizeSuiAddress("0x0");

/** `exchange::order::Order` economic fields (client-side, pre-signature). */
export type OrderFields = {
  /** Canonical `0x…::m::T` type string of the coin the MAKER pays out. */
  makerToken: string;
  /** Canonical type string of the coin the maker receives (taker pays). */
  takerToken: string;
  makerAmount: bigint;
  takerAmount: bigint;
  maxFeeBps: bigint;
  maker: string;
  /** The maker's exchange `BalanceManager` (escrow) object id. */
  makerManagerId: string;
  /** `0x0` = any taker. */
  taker: string;
  /** `0x0` = any relayer may settle. */
  sender: string;
  expiryMs: number;
  salt: bigint;
};

// BCS mirror of Move `exchange::order::Order` — field order is
// consensus-critical (exchange-types/src/order.rs).
const OrderBcs = bcs.struct("Order", {
  makerToken: bcs.string(),
  takerToken: bcs.string(),
  makerAmount: bcs.u64(),
  takerAmount: bcs.u64(),
  maxFeeBps: bcs.u64(),
  maker: bcs.Address,
  makerManagerId: bcs.Address,
  taker: bcs.Address,
  sender: bcs.Address,
  expiryMs: bcs.u64(),
  salt: bcs.u64(),
});

function blake2b256(data: Uint8Array): Uint8Array {
  return blake2b(data, { dkLen: 32 });
}

/** The exact BCS payload hashed into the digest and passed on-chain as
 * `order_bytes`. */
export function orderBytes(o: OrderFields): Uint8Array {
  return OrderBcs.serialize({
    makerToken: o.makerToken,
    takerToken: o.takerToken,
    makerAmount: o.makerAmount,
    takerAmount: o.takerAmount,
    maxFeeBps: o.maxFeeBps,
    maker: o.maker,
    makerManagerId: o.makerManagerId,
    taker: o.taker,
    sender: o.sender,
    expiryMs: BigInt(o.expiryMs),
    salt: o.salt,
  }).toBytes();
}

/**
 * §4.2 order digest (pure, unit-testable):
 * `blake2b256(TAG ‖ VERSION ‖ bcs(registry_id) ‖ bcs(order))`. The registry
 * object id domain-binds the order to one market on one deployment.
 */
export function orderDigest(order: OrderFields, registryId: string): Uint8Array {
  const tag = new TextEncoder().encode(ORDER_DOMAIN_TAG);
  const registry = bcs.Address.serialize(registryId).toBytes();
  const body = orderBytes(order);
  const buf = new Uint8Array(tag.length + 1 + registry.length + body.length);
  buf.set(tag, 0);
  buf[tag.length] = ORDER_DOMAIN_VERSION;
  buf.set(registry, tag.length + 1);
  buf.set(body, tag.length + 1 + registry.length);
  return blake2b256(buf);
}

/** The personal-message payload a soft cancel signs: `CANCEL_TAG ‖ digest`. */
export function buildCancelMessage(digest: Uint8Array): Uint8Array {
  const tag = new TextEncoder().encode(CANCEL_DOMAIN_TAG);
  const out = new Uint8Array(tag.length + digest.length);
  out.set(tag, 0);
  out.set(digest, tag.length);
  return out;
}

export function digestHex(digest: Uint8Array): string {
  return "0x" + Array.from(digest, (b) => b.toString(16).padStart(2, "0")).join("");
}

/** Parse a `0x…` (or bare) 64-hex digest back to its 32 bytes. */
export function digestFromHex(hex: string): Uint8Array {
  const clean = hex.startsWith("0x") ? hex.slice(2) : hex;
  if (!/^[0-9a-fA-F]{64}$/.test(clean)) {
    throw new Error(`malformed order digest: ${hex}`);
  }
  const out = new Uint8Array(32);
  for (let i = 0; i < 32; i++) out[i] = parseInt(clean.slice(i * 2, i * 2 + 2), 16);
  return out;
}

// ---- wallet signature plumbing ----------------------------------------------

/** Signature schemes the exchange accepts (Sui flag byte = wire value). */
export type ExchangeScheme = "ed25519" | "secp256k1";

/**
 * Split a wallet-standard serialized signature (`flag ‖ sig ‖ pubkey`,
 * base64 — what `signPersonalMessage` returns) into the orderbook wire
 * fields. Multisig / zkLogin wallets can't maker-sign exchange orders (the
 * chain derives a single-key address from the pubkey), so anything but
 * ed25519/secp256k1 is rejected.
 */
export function splitWalletSignature(serializedB64: string): {
  scheme: ExchangeScheme;
  /** base64 raw 64-byte signature */
  signature: string;
  /** base64 public key (32 ed25519 / 33 secp256k1) */
  publicKey: string;
} {
  const raw = fromBase64(serializedB64);
  const flag = raw[0];
  const scheme: ExchangeScheme | null =
    flag === 0x00 ? "ed25519" : flag === 0x01 ? "secp256k1" : null;
  if (!scheme) {
    throw new Error(
      `unsupported wallet signature scheme (flag ${flag}) — exchange orders need an ed25519 or secp256k1 wallet`,
    );
  }
  const pkLen = scheme === "ed25519" ? 32 : 33;
  if (raw.length !== 1 + 64 + pkLen) {
    throw new Error(`malformed wallet signature (${raw.length} bytes)`);
  }
  const toB64 = (b: Uint8Array) => btoa(String.fromCharCode(...b));
  return {
    scheme,
    signature: toB64(raw.slice(1, 65)),
    publicKey: toB64(raw.slice(65)),
  };
}

// ---- on-chain fill arguments ------------------------------------------------

/** On-chain signature argument: scheme flag byte ‖ raw 64-byte signature. */
export function prefixedSignature(schemeFlag: ExchangeScheme, signatureB64: string): Uint8Array {
  const raw = fromBase64(signatureB64);
  const out = new Uint8Array(raw.length + 1);
  out[0] = schemeFlag === "ed25519" ? 0x00 : 0x01;
  out.set(raw, 1);
  return out;
}

export function publicKeyBytes(publicKeyB64: string): Uint8Array {
  return fromBase64(publicKeyB64);
}
