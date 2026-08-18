// Programmable Transaction Block builders for cash-secured PUT dashboard actions.
//
// Mirror of `tx/dashboard.ts` (covered calls), targeting the `put_bucket`
// module. Shapes mirror the Move signatures in
// `contracts/sources/put_bucket.move`:
//   - put_bucket::exercise<U, S, Put>(bucket, put_coin: Coin<Put>,
//       underlying_delivery: Coin<Underlying>, clock): Coin<Settlement>
//   - put_bucket::redeem_position<U, S, Put>(bucket, position, clock)
//
// Put exercise is the mirror of call exercise: the holder burns `Coin<Put>`
// AND delivers `amount` underlying, and receives floor(amount*strike)
// settlement out (which we transfer back to the wallet).

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

export type ExercisePutParams = {
  bucketId: string;
  /** The bucket's per-bucket option coin type (`Put` type arg). */
  putCoinType: string;
  /** Amount to exercise, in underlying smallest units (== option coin units). */
  exerciseAmountRaw: bigint;
  /** Underlying to deliver, in underlying smallest units. `= exerciseAmountRaw`. */
  underlyingDeliveryRaw: bigint;
  underlyingCoinType: string;
  settlementCoinType: string;
  /** Recipient for the settlement that comes out of `exercise`. Usually the user's own address. */
  recipient: string;
};

/**
 * Build a PTB that exercises `exerciseAmountRaw` of the bucket's put option coin.
 *
 * `coinWithBalance` selects/splits exactly `exerciseAmountRaw` of the option
 * coin (`Coin<Put>`) from the user's holdings — partial exercise needs no
 * special-casing — and pulls the underlying delivery coin. The settlement
 * returned by `put_bucket::exercise` is transferred to the recipient. Option
 * coins always live in the wallet on the exchange path (SO-416).
 */
export function buildExercisePutTx(p: ExercisePutParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();

  // Exact option coin to burn, selected/split from the wallet's holdings.
  const putCoin = tx.add(
    coinWithBalance({ balance: p.exerciseAmountRaw, type: p.putCoinType }),
  );

  // Exact underlying delivery Coin out of the user's holdings.
  const underlyingDelivery = tx.add(
    coinWithBalance({
      balance: p.underlyingDeliveryRaw,
      type: p.underlyingCoinType,
    }),
  );

  const settlement = tx.moveCall({
    target: `${pkg}::put_bucket::exercise`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.putCoinType],
    arguments: [
      tx.object(p.bucketId),
      putCoin,
      underlyingDelivery,
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });

  tx.transferObjects([settlement], p.recipient);
  return tx;
}

export type RedeemPutParams = {
  bucketId: string;
  positionObjectId: string;
  /** The bucket's per-bucket option coin type (`Put` type arg). */
  putCoinType: string;
  underlyingCoinType: string;
  settlementCoinType: string;
  recipient: string;
};

/**
 * Build a PTB that burns the writer's `Position` and returns their share of
 * the put bucket's balances: the exercised range returns underlying, the
 * unexercised range returns settlement.
 */
export function buildRedeemPutTx(p: RedeemPutParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();

  const [underlying, settlement] = tx.moveCall({
    target: `${pkg}::put_bucket::redeem_position`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.putCoinType],
    arguments: [
      tx.object(p.bucketId),
      tx.object(p.positionObjectId),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });

  tx.transferObjects([underlying, settlement], p.recipient);
  return tx;
}
