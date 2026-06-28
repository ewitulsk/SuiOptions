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
import type { TransactionObjectArgument } from "@mysten/sui/transactions";

import { DEEPBOOK_PACKAGE_ID, ENV, PACKAGE_ID } from "../config";

function requirePackage(): string {
  if (!PACKAGE_ID) {
    throw new Error(
      `No deployment for VITE_ENVIRONMENT="${ENV}" (token-info returned no packageId) — the dashboard cannot build PTBs against the protocol`,
    );
  }
  return PACKAGE_ID;
}

/** Withdraw the option token out of the DeepBook trading account inline,
 *  so a single exercise PTB can consume tokens the user parked in the BM. */
export type BmWithdraw = {
  poolId: string;
  bmId: string;
  /** The wallet's own balance of the option coin (0 when it's entirely in the BM). */
  walletAmountRaw: bigint;
};

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
  /** When set, settle + withdraw the option coin from the BM first, then fold
   *  it into the exercised coin (the user left it in their trading account). */
  bmWithdraw?: BmWithdraw;
};

/**
 * Build a PTB that exercises `exerciseAmountRaw` of the bucket's put option coin.
 *
 * `coinWithBalance` selects/splits exactly `exerciseAmountRaw` of the option
 * coin (`Coin<Put>`) from the user's holdings — partial exercise needs no
 * special-casing — and pulls the underlying delivery coin. The settlement
 * returned by `put_bucket::exercise` is transferred to the recipient.
 *
 * When `bmWithdraw` is set the option coin lives (partly or wholly) in the
 * user's DeepBook trading account: the PTB first settles + withdraws it out of
 * the BM, merges it with any wallet holding, then splits off the exact
 * exercise amount.
 */
export function buildExercisePutTx(p: ExercisePutParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();

  let putCoin: TransactionObjectArgument;
  if (p.bmWithdraw) {
    if (!DEEPBOOK_PACKAGE_ID) {
      throw new Error("no DeepBook deployment — cannot withdraw the option coin from the trading account");
    }
    const { poolId, bmId, walletAmountRaw } = p.bmWithdraw;
    // Settle pending fills into the BM, then pull the option coin out.
    const proof = tx.moveCall({
      target: `${DEEPBOOK_PACKAGE_ID}::balance_manager::generate_proof_as_owner`,
      arguments: [tx.object(bmId)],
    });
    tx.moveCall({
      target: `${DEEPBOOK_PACKAGE_ID}::pool::withdraw_settled_amounts`,
      typeArguments: [p.putCoinType, p.settlementCoinType],
      arguments: [tx.object(poolId), tx.object(bmId), proof],
    });
    const bmCoin = tx.moveCall({
      target: `${DEEPBOOK_PACKAGE_ID}::balance_manager::withdraw_all`,
      typeArguments: [p.putCoinType],
      arguments: [tx.object(bmId)],
    });
    // Source = wallet holding (if any) merged with the BM coin. Split the exact
    // exercise amount out of it; transfer the remainder back to the user.
    let source: TransactionObjectArgument;
    if (walletAmountRaw > 0n) {
      const walletCoin = tx.add(
        coinWithBalance({ balance: walletAmountRaw, type: p.putCoinType }),
      );
      tx.mergeCoins(walletCoin, [bmCoin]);
      source = walletCoin;
    } else {
      source = bmCoin;
    }
    [putCoin] = tx.splitCoins(source, [p.exerciseAmountRaw]);
    tx.transferObjects([source], p.recipient);
  } else {
    // Exact option coin to burn, selected/split from the wallet's holdings.
    putCoin = tx.add(
      coinWithBalance({ balance: p.exerciseAmountRaw, type: p.putCoinType }),
    );
  }

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
