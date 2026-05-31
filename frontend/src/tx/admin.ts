// Programmable Transaction Block builders for the admin page.
//
// Shapes mirror the Move signatures in:
//   contracts/sources/admin.move
//     - admin::set_fee_bps(&AdminCap, &mut ProtocolConfig, new_bps)
//   contracts/sources/bucket.move
//     - bucket::new_call_option<U, S>(&AdminCap, expiry_ms, start_strike,
//         strike_interval, count, strike_scale, ctx)
//     - bucket::invalidate_bucket<U, S>(&AdminCap, &mut Bucket, reason, &Clock, ctx)
//     - bucket::revalidate_bucket<U, S>(&AdminCap, &mut Bucket, reason, &Clock, ctx)
//     - bucket::cleanup_bucket<U, S>(&AdminCap, Bucket, &Clock)
//   contracts/sources/treasury.move
//     - treasury::withdraw<T>(&AdminCap, &mut Treasury, amount, recipient, ctx)
//     - treasury::create_and_share(&AdminCap, ctx)
//
// Every admin call passes the caller's owned `AdminCap` object id (see
// `useAdminCap`) as its authorizing argument. Shared objects (Bucket,
// ProtocolConfig, Treasury) are passed by id; dapp-kit's SuiClient
// resolves their shared metadata.

import { Transaction } from "@mysten/sui/transactions";
import { SUI_CLOCK_OBJECT_ID } from "@mysten/sui/utils";

const PACKAGE_ID: string | undefined = import.meta.env.VITE_PACKAGE_ID as
  | string
  | undefined;

function requirePackage(): string {
  if (!PACKAGE_ID) {
    throw new Error(
      "VITE_PACKAGE_ID is not set — the admin page cannot build PTBs against the protocol",
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
  reason: string;
};

export function buildInvalidateBucketTx(p: BucketGateParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::bucket::invalidate_bucket`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType],
    arguments: [
      tx.object(p.adminCapId),
      tx.object(p.bucketId),
      tx.pure.vector("u8", reasonBytes(p.reason)),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
  return tx;
}

export function buildRevalidateBucketTx(p: BucketGateParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::bucket::revalidate_bucket`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType],
    arguments: [
      tx.object(p.adminCapId),
      tx.object(p.bucketId),
      tx.pure.vector("u8", reasonBytes(p.reason)),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
  return tx;
}

export type CleanupBucketParams = {
  adminCapId: string;
  bucketId: string;
  underlyingCoinType: string;
  settlementCoinType: string;
};

export function buildCleanupBucketTx(p: CleanupBucketParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  // `cleanup_bucket` takes the Bucket by value (deletes it). It still
  // resolves as a shared-object arg from its id.
  tx.moveCall({
    target: `${pkg}::bucket::cleanup_bucket`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType],
    arguments: [
      tx.object(p.adminCapId),
      tx.object(p.bucketId),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
  return tx;
}

export type NewCallOptionParams = {
  adminCapId: string;
  underlyingCoinType: string;
  settlementCoinType: string;
  /** Unix millis expiry. */
  expiryMs: bigint;
  /** First strike, in scaled chain units (raw u128). */
  startStrikeRaw: bigint;
  /** Spacing between consecutive strikes, in the same scaled units. */
  strikeIntervalRaw: bigint;
  /** Number of buckets to mint across the strike ladder. */
  count: bigint;
  /** Real ratio = strike / 10^strike_scale. `0 ≤ scale ≤ 38`. */
  strikeScale: number;
};

export function buildNewCallOptionTx(p: NewCallOptionParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::bucket::new_call_option`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType],
    arguments: [
      tx.object(p.adminCapId),
      tx.pure.u64(p.expiryMs),
      tx.pure.u128(p.startStrikeRaw),
      tx.pure.u128(p.strikeIntervalRaw),
      tx.pure.u64(p.count),
      tx.pure.u8(p.strikeScale),
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
    target: `${pkg}::admin::set_fee_bps`,
    arguments: [
      tx.object(p.adminCapId),
      tx.object(p.protocolConfigId),
      tx.pure.u64(p.newBps),
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
  tx.moveCall({
    target: `${pkg}::treasury::withdraw`,
    typeArguments: [p.coinType],
    arguments: [
      tx.object(p.adminCapId),
      tx.object(p.treasuryId),
      tx.pure.u64(p.amountRaw),
      tx.pure.address(p.recipient),
    ],
  });
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
