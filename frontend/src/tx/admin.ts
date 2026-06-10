// Programmable Transaction Block builders for the admin page.
//
// Two authority levels exist post-orgs:
//   - Platform admin (`AdminCap`): protocol fee, pause, treasury, and
//     emergency invalidate/revalidate overrides on ANY org's bucket.
//   - Org admin (`OrgCap`): gates + cleanup on the org's own buckets —
//     see `tx/org.ts`.
//
// Shapes mirror the Move signatures in:
//   contracts/sources/admin.move
//     - admin::set_protocol_fee_bps(&AdminCap, &mut ProtocolConfig, new_bps)
//     - admin::set_pause(&AdminCap, &mut ProtocolConfig, paused, ctx)
//   contracts/sources/bucket.move
//     - bucket::admin_invalidate_bucket<U, S, Call>(&AdminCap, &mut Bucket, reason, &Clock, ctx)
//     - bucket::admin_revalidate_bucket<U, S, Call>(&AdminCap, &mut Bucket, reason, &Clock, ctx)
//   contracts/sources/treasury.move
//     - treasury::withdraw<T>(&AdminCap, &mut Treasury, amount, ctx): Coin<T>
//     - treasury::create_and_share(&AdminCap, ctx)
//
// `treasury::withdraw` RETURNS the coin — the PTB transfers it onward.
// Shared objects (Bucket, ProtocolConfig, Treasury) are passed by id;
// dapp-kit's SuiClient resolves their shared metadata.

import { Transaction } from "@mysten/sui/transactions";
import { SUI_CLOCK_OBJECT_ID } from "@mysten/sui/utils";

import { ENV, PACKAGE_ID } from "../config";

function requirePackage(): string {
  if (!PACKAGE_ID) {
    throw new Error(
      `No deployment for VITE_ENVIRONMENT="${ENV}" (token-info returned no packageId) — the admin page cannot build PTBs against the protocol`,
    );
  }
  return PACKAGE_ID;
}

/** `reason: vector<u8>` arg — UTF-8 bytes of the admin's note. */
function reasonBytes(reason: string): number[] {
  return Array.from(new TextEncoder().encode(reason));
}

export type BucketGateParams = {
  adminCapId: string;
  bucketId: string;
  underlyingCoinType: string;
  settlementCoinType: string;
  /** The bucket's per-bucket option coin type (`Call` type arg). */
  callCoinType: string;
  reason: string;
};

/** Platform-admin override: invalidate ANY org's bucket. */
export function buildInvalidateBucketTx(p: BucketGateParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::bucket::admin_invalidate_bucket`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.callCoinType],
    arguments: [
      tx.object(p.adminCapId),
      tx.object(p.bucketId),
      tx.pure.vector("u8", reasonBytes(p.reason)),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
  return tx;
}

/** Platform-admin override: revalidate ANY org's bucket. */
export function buildRevalidateBucketTx(p: BucketGateParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::bucket::admin_revalidate_bucket`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.callCoinType],
    arguments: [
      tx.object(p.adminCapId),
      tx.object(p.bucketId),
      tx.pure.vector("u8", reasonBytes(p.reason)),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
  return tx;
}

export type SetFeeBpsParams = {
  adminCapId: string;
  protocolConfigId: string;
  newBps: bigint;
};

export function buildSetFeeBpsTx(p: SetFeeBpsParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::admin::set_protocol_fee_bps`,
    arguments: [
      tx.object(p.adminCapId),
      tx.object(p.protocolConfigId),
      tx.pure.u64(p.newBps),
    ],
  });
  return tx;
}

export type SetPauseParams = {
  adminCapId: string;
  protocolConfigId: string;
  paused: boolean;
};

/** Protocol-wide emergency brake: blocks NEW WRITES only across all orgs. */
export function buildSetPauseTx(p: SetPauseParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::admin::set_pause`,
    arguments: [
      tx.object(p.adminCapId),
      tx.object(p.protocolConfigId),
      tx.pure.bool(p.paused),
    ],
  });
  return tx;
}

export type WithdrawParams = {
  adminCapId: string;
  treasuryId: string;
  /** Move type `T` of the balance to withdraw, e.g. `0x…::tusdc::TUSDC`. */
  coinType: string;
  /** Amount in the coin's smallest units (raw u64). */
  amountRaw: bigint;
  recipient: string;
};

export function buildWithdrawTx(p: WithdrawParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  // `withdraw` returns the coin; the PTB routes it to the recipient.
  const [coin] = tx.moveCall({
    target: `${pkg}::treasury::withdraw`,
    typeArguments: [p.coinType],
    arguments: [
      tx.object(p.adminCapId),
      tx.object(p.treasuryId),
      tx.pure.u64(p.amountRaw),
    ],
  });
  tx.transferObjects([coin], p.recipient);
  return tx;
}

export function buildCreateTreasuryTx(adminCapId: string): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::treasury::create_and_share`,
    arguments: [tx.object(adminCapId)],
  });
  return tx;
}
