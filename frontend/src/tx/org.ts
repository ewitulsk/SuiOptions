// Programmable Transaction Block builders for org operations.
//
// Orgs are PERMISSIONLESS: any wallet can create one. The org's `OrgCap`
// (returned by `create_org`) is the sole authority over the org — fee
// setting, fee withdrawal, and bucket lifecycle (invalidate / revalidate /
// cleanup) on the org's own buckets.
//
// Shapes mirror the Move signatures in:
//   contracts/sources/org.move
//     - org::create_org(name, fee_bps, ctx): OrgCap
//     - org::set_org_fee_bps(&OrgCap, &mut Org, new_bps)
//     - org::withdraw<T>(&OrgCap, &mut Org, amount, ctx): Coin<T>
//   contracts/sources/bucket.move
//     - bucket::invalidate_bucket<U, S, Call>(&OrgCap, &mut Bucket, reason, &Clock, ctx)
//     - bucket::revalidate_bucket<U, S, Call>(&OrgCap, &mut Bucket, reason, &Clock, ctx)
//     - bucket::cleanup_bucket<U, S, Call>(&OrgCap, Bucket, &Clock): TreasuryCap<Call>
//
// `create_org` / `withdraw` / `cleanup_bucket` RETURN values, which the PTB
// must route (the wallet sender keeps them here).

import { Transaction } from "@mysten/sui/transactions";
import { SUI_CLOCK_OBJECT_ID } from "@mysten/sui/utils";

import { ENV, PACKAGE_ID } from "../config";

function requirePackage(): string {
  if (!PACKAGE_ID) {
    throw new Error(
      `No deployment for VITE_ENVIRONMENT="${ENV}" (token-info returned no packageId) — cannot build org PTBs`,
    );
  }
  return PACKAGE_ID;
}

function reasonBytes(reason: string): number[] {
  return Array.from(new TextEncoder().encode(reason));
}

export type CreateOrgParams = {
  /** Display name, 1..=64 bytes. Cosmetic — the org id is the identity. */
  name: string;
  /** Org fee in basis points of gross premium (0..=1000). */
  feeBps: bigint;
  /** Wallet that receives the returned OrgCap (usually the sender). */
  capRecipient: string;
};

export function buildCreateOrgTx(p: CreateOrgParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  const [cap] = tx.moveCall({
    target: `${pkg}::org::create_org`,
    arguments: [tx.pure.string(p.name), tx.pure.u64(p.feeBps)],
  });
  tx.transferObjects([cap], p.capRecipient);
  return tx;
}

export type SetOrgFeeParams = {
  orgCapId: string;
  orgId: string;
  newBps: bigint;
};

export function buildSetOrgFeeTx(p: SetOrgFeeParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::org::set_org_fee_bps`,
    arguments: [tx.object(p.orgCapId), tx.object(p.orgId), tx.pure.u64(p.newBps)],
  });
  return tx;
}

export type OrgWithdrawParams = {
  orgCapId: string;
  orgId: string;
  /** Move type `T` of the balance to withdraw. */
  coinType: string;
  /** Amount in the coin's smallest units (raw u64). */
  amountRaw: bigint;
  recipient: string;
};

export function buildOrgWithdrawTx(p: OrgWithdrawParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  const [coin] = tx.moveCall({
    target: `${pkg}::org::withdraw`,
    typeArguments: [p.coinType],
    arguments: [tx.object(p.orgCapId), tx.object(p.orgId), tx.pure.u64(p.amountRaw)],
  });
  tx.transferObjects([coin], p.recipient);
  return tx;
}

export type OrgBucketGateParams = {
  orgCapId: string;
  bucketId: string;
  underlyingCoinType: string;
  settlementCoinType: string;
  /** The bucket's per-bucket option coin type (`Call` type arg). */
  callCoinType: string;
  reason: string;
};

export function buildOrgInvalidateBucketTx(p: OrgBucketGateParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::bucket::invalidate_bucket`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.callCoinType],
    arguments: [
      tx.object(p.orgCapId),
      tx.object(p.bucketId),
      tx.pure.vector("u8", reasonBytes(p.reason)),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
  return tx;
}

export function buildOrgRevalidateBucketTx(p: OrgBucketGateParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::bucket::revalidate_bucket`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.callCoinType],
    arguments: [
      tx.object(p.orgCapId),
      tx.object(p.bucketId),
      tx.pure.vector("u8", reasonBytes(p.reason)),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
  return tx;
}

export type OrgCleanupBucketParams = {
  orgCapId: string;
  bucketId: string;
  underlyingCoinType: string;
  settlementCoinType: string;
  /** The bucket's per-bucket option coin type (`Call` type arg). */
  callCoinType: string;
  /** Wallet that keeps the returned TreasuryCap<Call> (usually the sender). */
  capRecipient: string;
};

export function buildOrgCleanupBucketTx(p: OrgCleanupBucketParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  // `cleanup_bucket` takes the Bucket by value (deletes it) and RETURNS the
  // option coin's TreasuryCap — outstanding option coins may still exist, so
  // the cap can't be dropped on-chain. Route it to the org admin's wallet.
  const [treasuryCap] = tx.moveCall({
    target: `${pkg}::bucket::cleanup_bucket`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.callCoinType],
    arguments: [
      tx.object(p.orgCapId),
      tx.object(p.bucketId),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
  tx.transferObjects([treasuryCap], p.capRecipient);
  return tx;
}
