// Programmable Transaction Block builders for dashboard actions.
//
// Shapes mirror the Move signatures in `contracts/sources/bucket.move`:
//   - bucket::exercise<U, S>(bucket, call, settlement_payment, clock, ctx)
//   - bucket::redeem_position<U, S>(bucket, position, clock, ctx)
//
// Both rely on the dapp-kit `SuiClient` to resolve shared-object metadata
// (initial_shared_version, mutability) so callers just pass object ids.

import { Transaction } from "@mysten/sui/transactions";
import { coinWithBalance } from "@mysten/sui/transactions";
import { SUI_CLOCK_OBJECT_ID } from "@mysten/sui/utils";

import { ENV, PACKAGE_ID } from "../config";

function requirePackage(): string {
  if (!PACKAGE_ID) {
    throw new Error(
      `No deployment for VITE_ENVIRONMENT="${ENV}" in deployments.json — the dashboard cannot build PTBs against the protocol`,
    );
  }
  return PACKAGE_ID;
}

export type ExerciseParams = {
  bucketId: string;
  callOptionId: string;
  /** Total amount inside the `CallOption` object, in underlying smallest units. */
  fullAmountRaw: bigint;
  /** Amount to exercise, in underlying smallest units. `≤ fullAmountRaw`. */
  exerciseAmountRaw: bigint;
  /** Settlement to pay, in settlement-asset smallest units. `= amount * strike` per §3.3.5. */
  settlementAmountRaw: bigint;
  underlyingCoinType: string;
  settlementCoinType: string;
  /** Recipient for the underlying that comes out of `exercise`. Usually the user's own address. */
  recipient: string;
};

/**
 * Build a PTB that exercises `exerciseAmountRaw` of a `CallOption`.
 *
 * If the user wants to exercise less than the whole object, we split first
 * via `call_option::split` and pass the carved child into `exercise`. The
 * remainder stays in the user's wallet as the original object (now with
 * reduced `amount`).
 */
export function buildExerciseTx(p: ExerciseParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();

  const partial = p.exerciseAmountRaw < p.fullAmountRaw;
  const callArg = partial
    ? tx.moveCall({
        target: `${pkg}::call_option::split`,
        arguments: [tx.object(p.callOptionId), tx.pure.u64(p.exerciseAmountRaw)],
      })
    : tx.object(p.callOptionId);

  // Pull the exact settlement Coin out of the user's USDC holdings.
  // `coinWithBalance` handles merging/splitting across multiple coin objects.
  const settlement = tx.add(
    coinWithBalance({
      balance: p.settlementAmountRaw,
      type: p.settlementCoinType,
    }),
  );

  const underlying = tx.moveCall({
    target: `${pkg}::bucket::exercise`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType],
    arguments: [
      tx.object(p.bucketId),
      callArg,
      settlement,
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });

  tx.transferObjects([underlying], p.recipient);
  return tx;
}

export type RedeemParams = {
  bucketId: string;
  positionObjectId: string;
  underlyingCoinType: string;
  settlementCoinType: string;
  recipient: string;
};

/**
 * Build a PTB that burns the writer's `Position` and returns their share
 * of the bucket's underlying + settlement balances (§3.3.6).
 */
export function buildRedeemTx(p: RedeemParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();

  const [underlying, settlement] = tx.moveCall({
    target: `${pkg}::bucket::redeem_position`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType],
    arguments: [
      tx.object(p.bucketId),
      tx.object(p.positionObjectId),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });

  tx.transferObjects([underlying, settlement], p.recipient);
  return tx;
}
