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

/** Withdraw the position token out of the DeepBook trading account inline,
 *  so a single exercise PTB can consume tokens the user parked in the BM. */
export type BmWithdraw = {
  poolId: string;
  bmId: string;
  /** The wallet's own balance of the option coin (0 when it's entirely in the BM). */
  walletAmountRaw: bigint;
};

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
  /** When set, settle + withdraw the option coin from the BM first, then fold
   *  it into the exercised coin (the user left it in their trading account). */
  bmWithdraw?: BmWithdraw;
};

/**
 * Build a PTB that exercises `exerciseAmountRaw` of the bucket's option coin.
 *
 * `coinWithBalance` selects/splits exactly `exerciseAmountRaw` of the option
 * coin (`Coin<Call>`) from the user's holdings — partial exercise needs no
 * special-casing — and the same helper pulls the settlement payment.
 *
 * When `bmWithdraw` is set the option coin lives (partly or wholly) in the
 * user's DeepBook trading account: the PTB first settles + withdraws it out of
 * the BM, merges it with any wallet holding, then splits off the exact
 * exercise amount. `bucket::exercise` burns the *entire* coin it's handed
 * (`bucket.move`), so we always split to an exact amount and hand the remainder
 * back to the user.
 */
export function buildExerciseTx(p: ExerciseParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();

  let callCoin: TransactionObjectArgument;
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
      typeArguments: [p.callCoinType, p.settlementCoinType],
      arguments: [tx.object(poolId), tx.object(bmId), proof],
    });
    const bmCoin = tx.moveCall({
      target: `${DEEPBOOK_PACKAGE_ID}::balance_manager::withdraw_all`,
      typeArguments: [p.callCoinType],
      arguments: [tx.object(bmId)],
    });
    // Source = wallet holding (if any) merged with the BM coin. Split the exact
    // exercise amount out of it; transfer the remainder back to the user.
    let source: TransactionObjectArgument;
    if (walletAmountRaw > 0n) {
      const walletCoin = tx.add(
        coinWithBalance({ balance: walletAmountRaw, type: p.callCoinType }),
      );
      tx.mergeCoins(walletCoin, [bmCoin]);
      source = walletCoin;
    } else {
      source = bmCoin;
    }
    [callCoin] = tx.splitCoins(source, [p.exerciseAmountRaw]);
    tx.transferObjects([source], p.recipient);
  } else {
    // Exact option coin to burn, selected/split from the wallet's holdings.
    callCoin = tx.add(
      coinWithBalance({ balance: p.exerciseAmountRaw, type: p.callCoinType }),
    );
  }

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
