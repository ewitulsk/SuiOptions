// BCS mirror of Move `exchange::order::Order` — field order is
// consensus-critical and must match contracts/exchange/sources/order.move
// `from_bytes` / rust-backend/crates/exchange-types/src/order.rs exactly.
// Token types are canonical "0x" + 64-hex strings on both sides.

import { bcs } from "@mysten/sui/bcs";
import { fromBase64 } from "@mysten/sui/utils";

import type { SignedOrder } from "../api/orderbook";
import { toBigint } from "../format";

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

/** The exact payload passed on-chain as `order_bytes` (and hashed for the digest). */
export function orderBytes(o: SignedOrder): Uint8Array {
  return OrderBcs.serialize({
    makerToken: o.makerToken,
    takerToken: o.takerToken,
    makerAmount: toBigint(o.makerAmount),
    takerAmount: toBigint(o.takerAmount),
    maxFeeBps: toBigint(o.maxFeeBps),
    maker: o.maker,
    makerManagerId: o.makerManagerId,
    taker: o.taker,
    sender: o.sender,
    expiryMs: BigInt(o.expiryMs),
    salt: toBigint(o.salt),
  }).toBytes();
}

/** On-chain signature argument: scheme flag byte ++ raw signature. */
export function prefixedSignature(o: SignedOrder): Uint8Array {
  const flag = o.scheme === "ed25519" ? 0x00 : 0x01;
  const raw = fromBase64(o.signature);
  const out = new Uint8Array(raw.length + 1);
  out[0] = flag;
  out.set(raw, 1);
  return out;
}

export function publicKeyBytes(o: SignedOrder): Uint8Array {
  return fromBase64(o.publicKey);
}
