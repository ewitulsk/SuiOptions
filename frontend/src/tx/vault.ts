// Programmable Transaction Block builders for the covered-call vault's
// wallet-facing user flows.
//
// Shapes mirror the Move signatures in `contracts/sources/vault.move` and must
// stay in lockstep with the gas station's `vault_*` templates
// (`rust-backend/crates/sui-tx/src/tx/template.rs`). Each is a single call with
// the vault's three type args `<U, S, V>` (underlying, settlement, share);
// none take the clock. Returned objects/coins are transferred to the wallet.

import { Transaction, coinWithBalance } from "@mysten/sui/transactions";

import { ENV, PACKAGE_ID } from "../config";

function requirePackage(): string {
  if (!PACKAGE_ID) {
    throw new Error(
      `No deployment for VITE_ENVIRONMENT="${ENV}" (token-info returned no packageId) — cannot build vault PTBs`,
    );
  }
  return PACKAGE_ID;
}

/** The three vault type args, sourced from the `Vault` DTO. */
export type VaultTypes = {
  /** `underlying_coin_type` — the `U` type arg. */
  underlyingCoinType: string;
  /** `settlement_coin_type` — the `S` type arg. */
  settlementCoinType: string;
  /** `share_type` — the `V` type arg (and the share `Coin<V>` type). */
  shareType: string;
};

function typeArgs(t: VaultTypes): string[] {
  return [t.underlyingCoinType, t.settlementCoinType, t.shareType];
}

export type VaultDepositParams = VaultTypes & {
  vaultId: string;
  /** Underlying to deposit, in smallest units. */
  amountRaw: bigint;
  /** Connected wallet; receives the `DepositReceipt`. */
  recipient: string;
};

/**
 * `vault::deposit` — queue underlying for the next round; mint a
 * `DepositReceipt` to the wallet. The receipt is later redeemed for shares
 * (`claim_shares`) once the round finalizes, or cancelled
 * (`instant_withdraw_pending`) before the round starts.
 */
export function buildVaultDepositTx(p: VaultDepositParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  const coin = tx.add(
    coinWithBalance({ balance: p.amountRaw, type: p.underlyingCoinType }),
  );
  const receipt = tx.moveCall({
    target: `${pkg}::vault::deposit`,
    typeArguments: typeArgs(p),
    arguments: [tx.object(p.vaultId), coin],
  });
  tx.transferObjects([receipt], p.recipient);
  return tx;
}

export type VaultReceiptActionParams = VaultTypes & {
  vaultId: string;
  /** The `DepositReceipt` / `WithdrawReceipt` object id. */
  receiptId: string;
  recipient: string;
};

/**
 * `vault::claim_shares` — convert a finalized `DepositReceipt` into share
 * `Coin<V>` at `pps[round − 1]`, transferred to the wallet.
 */
export function buildClaimSharesTx(p: VaultReceiptActionParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  const shares = tx.moveCall({
    target: `${pkg}::vault::claim_shares`,
    typeArguments: typeArgs(p),
    arguments: [tx.object(p.vaultId), tx.object(p.receiptId)],
  });
  tx.transferObjects([shares], p.recipient);
  return tx;
}

export type VaultInitiateWithdrawParams = VaultTypes & {
  vaultId: string;
  /** Shares to escrow, in smallest units. */
  sharesRaw: bigint;
  /** Connected wallet; receives the `WithdrawReceipt`. */
  recipient: string;
};

/**
 * `vault::initiate_withdraw` — escrow share `Coin<V>` and mint a
 * `WithdrawReceipt`. The escrowed shares stay exposed to the current round's
 * P&L (Ribbon semantics); `complete_withdraw` pays out once it finalizes.
 */
export function buildInitiateWithdrawTx(p: VaultInitiateWithdrawParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  const shares = tx.add(
    coinWithBalance({ balance: p.sharesRaw, type: p.shareType }),
  );
  const receipt = tx.moveCall({
    target: `${pkg}::vault::initiate_withdraw`,
    typeArguments: typeArgs(p),
    arguments: [tx.object(p.vaultId), shares],
  });
  tx.transferObjects([receipt], p.recipient);
  return tx;
}

/**
 * `vault::complete_withdraw` — pay a finalized `WithdrawReceipt` at its locked
 * pps, returning underlying `Coin<U>` to the wallet.
 */
export function buildCompleteWithdrawTx(p: VaultReceiptActionParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  const owed = tx.moveCall({
    target: `${pkg}::vault::complete_withdraw`,
    typeArguments: typeArgs(p),
    arguments: [tx.object(p.vaultId), tx.object(p.receiptId)],
  });
  tx.transferObjects([owed], p.recipient);
  return tx;
}

/**
 * `vault::instant_withdraw_pending` — cancel a `DepositReceipt` whose round
 * hasn't started yet, refunding the underlying `Coin<U>` to the wallet.
 */
export function buildInstantWithdrawTx(p: VaultReceiptActionParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  const refund = tx.moveCall({
    target: `${pkg}::vault::instant_withdraw_pending`,
    typeArguments: typeArgs(p),
    arguments: [tx.object(p.vaultId), tx.object(p.receiptId)],
  });
  tx.transferObjects([refund], p.recipient);
  return tx;
}
