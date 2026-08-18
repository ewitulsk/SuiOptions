// Programmable Transaction Block builders for dashboard actions.
//
// Shapes mirror the Move signatures in `contracts/sources/bucket.move`:
//   - bucket::exercise<U, S, Call>(bucket, call_coin, settlement_payment, clock, ctx)
//   - bucket::redeem_position<U, S, Call>(bucket, position, clock, ctx)
//
// The option is a per-bucket fungible `Coin<Call>`, so `exercise` takes a coin
// (selected/split via `coinWithBalance`) rather than a `CallOption` object id,
// and every call carries the bucket's `Call` type as the third type arg.
// Both rely on the dapp-kit `SuiClient` to resolve shared-object metadata
// (initial_shared_version, mutability) so callers just pass object ids.

import { Transaction } from "@mysten/sui/transactions";
import { coinWithBalance } from "@mysten/sui/transactions";
import { SUI_CLOCK_OBJECT_ID } from "@mysten/sui/utils";

import { ENV, PACKAGE_ID } from "../config";

function requirePackage(): string {
  if (!PACKAGE_ID) {
    throw new Error(
      `No deployment for VITE_ENVIRONMENT="${ENV}" (token-info returned no packageId) — the dashboard cannot build PTBs against the protocol`,
    );
  }
  return PACKAGE_ID;
}

export type ExerciseParams = {
  bucketId: string;
  /** The bucket's per-bucket option coin type (`Call` type arg). */
  callCoinType: string;
  /** Amount to exercise, in underlying smallest units (== option coin units). */
  exerciseAmountRaw: bigint;
  /** Settlement to pay, in settlement-asset smallest units. `= amount * strike` per §3.3.5. */
  settlementAmountRaw: bigint;
  underlyingCoinType: string;
  settlementCoinType: string;
  /** Recipient for the underlying that comes out of `exercise`. Usually the user's own address. */
  recipient: string;
};

/**
 * Build a PTB that exercises `exerciseAmountRaw` of the bucket's option coin.
 *
 * `coinWithBalance` selects/splits exactly `exerciseAmountRaw` of the option
 * coin (`Coin<Call>`) from the user's holdings — partial exercise needs no
 * special-casing — and the same helper pulls the settlement payment. Option
 * coins always live in the wallet on the exchange path (taker fills settle
 * straight to it, SO-416), so there is no custody to drain first.
 */
export function buildExerciseTx(p: ExerciseParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();

  // Exact option coin to burn, selected/split from the wallet's holdings.
  const callCoin = tx.add(
    coinWithBalance({ balance: p.exerciseAmountRaw, type: p.callCoinType }),
  );

  // Exact settlement Coin out of the user's holdings.
  const settlement = tx.add(
    coinWithBalance({
      balance: p.settlementAmountRaw,
      type: p.settlementCoinType,
    }),
  );

  const underlying = tx.moveCall({
    target: `${pkg}::bucket::exercise`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.callCoinType],
    arguments: [
      tx.object(p.bucketId),
      callCoin,
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
  /** The bucket's per-bucket option coin type (`Call` type arg). */
  callCoinType: string;
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
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.callCoinType],
    arguments: [
      tx.object(p.bucketId),
      tx.object(p.positionObjectId),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });

  tx.transferObjects([underlying, settlement], p.recipient);
  return tx;
}
