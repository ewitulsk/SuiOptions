// Programmable Transaction Block builders for the admin page.
//
// Shapes mirror the Move signatures in:
//   contracts/sources/admin.move
//     - admin::set_fee_bps(&AdminCap, &mut ProtocolConfig, new_bps)
//   contracts/sources/bucket.move
//     - bucket::invalidate_bucket<U, S, Call>(&AdminCap, &mut Bucket, reason, &Clock, ctx)
//     - bucket::revalidate_bucket<U, S, Call>(&AdminCap, &mut Bucket, reason, &Clock, ctx)
//     - bucket::cleanup_bucket<U, S, Call>(&AdminCap, Bucket, &Clock)
//   contracts/sources/treasury.move
//     - treasury::withdraw<T>(&AdminCap, &mut Treasury, amount, recipient, ctx)
//     - treasury::create_and_share(&AdminCap, ctx)
//   contracts/sources/vault.move
//     - vault::pause_deposits<U, S, V>(&AdminCap, &mut Vault)
//     - vault::unpause_deposits<U, S, V>(&AdminCap, &mut Vault)
//
// Every admin call passes the caller's owned `AdminCap` object id (see
// `useAdminCap`) as its authorizing argument. Shared objects (Bucket,
// ProtocolConfig, Treasury) are passed by id; dapp-kit's SuiClient
// resolves their shared metadata.

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

export function buildInvalidateBucketTx(p: BucketGateParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::bucket::invalidate_bucket`,
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

export function buildRevalidateBucketTx(p: BucketGateParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::bucket::revalidate_bucket`,
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

export type CleanupBucketParams = {
  adminCapId: string;
  bucketId: string;
  underlyingCoinType: string;
  settlementCoinType: string;
  /** The bucket's per-bucket option coin type (`Call` type arg). */
  callCoinType: string;
};

export function buildCleanupBucketTx(p: CleanupBucketParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  // `cleanup_bucket` takes the Bucket by value (deletes it). It still
  // resolves as a shared-object arg from its id.
  tx.moveCall({
    target: `${pkg}::bucket::cleanup_bucket`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.callCoinType],
    arguments: [
      tx.object(p.adminCapId),
      tx.object(p.bucketId),
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

export type VaultPauseParams = {
  adminCapId: string;
  vaultId: string;
  /** The vault's `U` / `S` / `V` type args (from the api-service `Vault`). */
  underlyingCoinType: string;
  settlementCoinType: string;
  shareType: string;
};

function buildVaultPauseToggleTx(
  fn: "pause_deposits" | "unpause_deposits",
  p: VaultPauseParams,
): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::vault::${fn}`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.shareType],
    arguments: [tx.object(p.adminCapId), tx.object(p.vaultId)],
  });
  return tx;
}

/** Stop new deposits into a vault. Hard cutover: the backend then ignores it. */
export function buildPauseDepositsTx(p: VaultPauseParams): Transaction {
  return buildVaultPauseToggleTx("pause_deposits", p);
}

/** Re-open deposits on a previously paused vault. */
export function buildUnpauseDepositsTx(p: VaultPauseParams): Transaction {
  return buildVaultPauseToggleTx("unpause_deposits", p);
}
