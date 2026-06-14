// PTB builders for the session-gated (`_with_session`) protocol entrypoints.
//
// Shapes mirror the Move signatures in `contracts/sources/bucket.move` and
// must stay in lockstep with the gas station's `session_*` templates
// (`rust-backend/crates/sui-tx/src/tx/template.rs`). Unlike the wallet
// builders (`composer.ts` / `dashboard.ts`), no coins ride in the PTB: the
// user's options Account (custody vault) funds the executor side and
// receives the outputs, all gated by the SessionCap.

import type { Transaction } from "@mysten/sui/transactions";
import { SUI_CLOCK_OBJECT_ID, fromHex } from "@mysten/sui/utils";

import { ENV, PACKAGE_ID, PROTOCOL_CONFIG_ID, TREASURY_ID } from "../config";
import type { RfqQuoteEntry } from "../api/quoting";

function requirePackage(): string {
  if (!PACKAGE_ID) {
    throw new Error(
      `No deployment for VITE_ENVIRONMENT="${ENV}" (token-info returned no packageId) — cannot build session PTBs`,
    );
  }
  return PACKAGE_ID;
}

function strip0x(s: string): string {
  return s.startsWith("0x") ? s.slice(2) : s;
}

/** Ids every session PTB references. */
export type SessionCtx = {
  capId: string;
  /** The siws_session Account (generation + spend ledger). */
  accountId: string;
  /** The user's options Account (custody vault). */
  optionsAccountId: string;
};

export type SessionTradeParams = {
  entry: RfqQuoteEntry;
  underlyingCoinType: string;
  settlementCoinType: string;
  callCoinType: string;
  /** "writer" = Earn (FlowKind::Writer), "trader" = Buy (FlowKind::Trader). */
  flow: "writer" | "trader";
};

/**
 * Append a writer/trader-flow `execute_write_with_session` to `tx`. The
 * quote prelude (`new_quote` → `new_signed_quote`) is identical to the
 * wallet path — the struct must BCS-encode to the exact bytes the MM signed.
 */
export function addSessionTrade(
  tx: Transaction,
  ctx: SessionCtx,
  p: SessionTradeParams,
): void {
  const pkg = requirePackage();
  if (!PROTOCOL_CONFIG_ID || !TREASURY_ID) {
    throw new Error(
      `Missing protocolConfigId/treasuryId for VITE_ENVIRONMENT="${ENV}" — cannot build execute_write_with_session`,
    );
  }
  const q = p.entry.quote;

  const quoteArg = tx.moveCall({
    target: `${pkg}::quote::new_quote`,
    arguments: [
      tx.pure.vector("u8", Array.from(fromHex(strip0x(q.protocol_id)))),
      tx.pure.id(q.signer_account_id),
      tx.pure.address(q.signer_token_recipient),
      tx.pure.id(q.bucket_id),
      tx.pure.u64(BigInt(q.write_amount)),
      tx.pure.u64(BigInt(q.premium)),
      tx.pure.u64(BigInt(q.valid_until_ms)),
      tx.pure.u64(BigInt(q.nonce)),
    ],
  });
  const signedQuote = tx.moveCall({
    target: `${pkg}::quote::new_signed_quote`,
    arguments: [
      quoteArg,
      tx.pure.vector("u8", Array.from(fromHex(strip0x(p.entry.signature)))),
    ],
  });
  const flow = tx.moveCall({
    target: `${pkg}::bucket::${p.flow === "writer" ? "writer_flow" : "trader_flow"}`,
  });

  tx.moveCall({
    target: `${pkg}::session_bucket::execute_write_with_session`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.callCoinType],
    arguments: [
      tx.object(q.bucket_id),
      tx.object(PROTOCOL_CONFIG_ID),
      tx.object(TREASURY_ID),
      tx.object(q.signer_account_id), // MM Account (shared, mutable)
      tx.object(ctx.optionsAccountId),
      tx.object(ctx.capId),
      tx.object(ctx.accountId),
      flow,
      signedQuote,
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
}

export type SessionExerciseParams = {
  bucketId: string;
  callCoinType: string;
  underlyingCoinType: string;
  settlementCoinType: string;
  /** Amount to exercise, in underlying smallest units (== option coin units). */
  exerciseAmountRaw: bigint;
};

/**
 * Append `exercise_with_session` to `tx`: burns custodied call coins, pays
 * the strike from custodied settlement, credits the underlying back.
 */
export function addSessionExercise(
  tx: Transaction,
  ctx: SessionCtx,
  p: SessionExerciseParams,
): void {
  const pkg = requirePackage();
  tx.moveCall({
    target: `${pkg}::session_bucket::exercise_with_session`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.callCoinType],
    arguments: [
      tx.object(p.bucketId),
      tx.object(ctx.optionsAccountId),
      tx.object(ctx.capId),
      tx.object(ctx.accountId),
      tx.pure.u64(p.exerciseAmountRaw),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
}

export type SessionRedeemParams = {
  bucketId: string;
  positionObjectId: string;
  callCoinType: string;
  underlyingCoinType: string;
  settlementCoinType: string;
};

/**
 * Append `redeem_position_with_session` to `tx`: redeems a custodied
 * Position after expiry; both legs settle back into custody.
 */
export function addSessionRedeem(
  tx: Transaction,
  ctx: SessionCtx,
  p: SessionRedeemParams,
): void {
  const pkg = requirePackage();
  tx.moveCall({
    target: `${pkg}::session_bucket::redeem_position_with_session`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.callCoinType],
    arguments: [
      tx.object(p.bucketId),
      tx.object(ctx.optionsAccountId),
      tx.object(ctx.capId),
      tx.object(ctx.accountId),
      tx.pure.id(p.positionObjectId),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
}

// ─────────────────────────── covered-call vault ───────────────────────────
//
// Session twins of `vault::*` (`contracts/sources/session_vault.move`). The
// underlying/share coins are sourced from — and all outputs (receipts, claimed
// shares, refunds) land back in — the user's options Account custody, gated by
// the SessionCap. Argument order matches each `*_with_session` signature:
//   (vault, user_account, cap, session_account, <amount|receipt_id>, clock).

/** The three vault type args `<U, S, V>`. */
export type SessionVaultTypes = {
  underlyingCoinType: string;
  settlementCoinType: string;
  shareType: string;
};

function vaultTypeArgs(t: SessionVaultTypes): string[] {
  return [t.underlyingCoinType, t.settlementCoinType, t.shareType];
}

export type SessionVaultDepositParams = SessionVaultTypes & {
  vaultId: string;
  /** Underlying to deposit, smallest units (charged against the cap's limit). */
  amountRaw: bigint;
};

/** Append `deposit_with_session`: custodied underlying → vault; receipt custodied. */
export function addSessionVaultDeposit(
  tx: Transaction,
  ctx: SessionCtx,
  p: SessionVaultDepositParams,
): void {
  const pkg = requirePackage();
  tx.moveCall({
    target: `${pkg}::session_vault::deposit_with_session`,
    typeArguments: vaultTypeArgs(p),
    arguments: [
      tx.object(p.vaultId),
      tx.object(ctx.optionsAccountId),
      tx.object(ctx.capId),
      tx.object(ctx.accountId),
      tx.pure.u64(p.amountRaw),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
}

export type SessionVaultReceiptParams = SessionVaultTypes & {
  vaultId: string;
  /** The custodied `DepositReceipt` / `WithdrawReceipt` object id. */
  receiptId: string;
};

/** Append `claim_shares_with_session`: custodied receipt → shares into custody. */
export function addSessionVaultClaimShares(
  tx: Transaction,
  ctx: SessionCtx,
  p: SessionVaultReceiptParams,
): void {
  const pkg = requirePackage();
  tx.moveCall({
    target: `${pkg}::session_vault::claim_shares_with_session`,
    typeArguments: vaultTypeArgs(p),
    arguments: [
      tx.object(p.vaultId),
      tx.object(ctx.optionsAccountId),
      tx.object(ctx.capId),
      tx.object(ctx.accountId),
      tx.pure.id(p.receiptId),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
}

export type SessionVaultInitiateWithdrawParams = SessionVaultTypes & {
  vaultId: string;
  /** Shares to escrow, smallest units. */
  sharesRaw: bigint;
};

/** Append `initiate_withdraw_with_session`: custodied shares escrowed; receipt custodied. */
export function addSessionVaultInitiateWithdraw(
  tx: Transaction,
  ctx: SessionCtx,
  p: SessionVaultInitiateWithdrawParams,
): void {
  const pkg = requirePackage();
  tx.moveCall({
    target: `${pkg}::session_vault::initiate_withdraw_with_session`,
    typeArguments: vaultTypeArgs(p),
    arguments: [
      tx.object(p.vaultId),
      tx.object(ctx.optionsAccountId),
      tx.object(ctx.capId),
      tx.object(ctx.accountId),
      tx.pure.u64(p.sharesRaw),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
}

/** Append `complete_withdraw_with_session`: finalized withdrawal paid into custody. */
export function addSessionVaultCompleteWithdraw(
  tx: Transaction,
  ctx: SessionCtx,
  p: SessionVaultReceiptParams,
): void {
  const pkg = requirePackage();
  tx.moveCall({
    target: `${pkg}::session_vault::complete_withdraw_with_session`,
    typeArguments: vaultTypeArgs(p),
    arguments: [
      tx.object(p.vaultId),
      tx.object(ctx.optionsAccountId),
      tx.object(ctx.capId),
      tx.object(ctx.accountId),
      tx.pure.id(p.receiptId),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
}

/** Append `instant_withdraw_pending_with_session`: cancel a queued deposit; refund into custody. */
export function addSessionVaultInstantWithdraw(
  tx: Transaction,
  ctx: SessionCtx,
  p: SessionVaultReceiptParams,
): void {
  const pkg = requirePackage();
  tx.moveCall({
    target: `${pkg}::session_vault::instant_withdraw_pending_with_session`,
    typeArguments: vaultTypeArgs(p),
    arguments: [
      tx.object(p.vaultId),
      tx.object(ctx.optionsAccountId),
      tx.object(ctx.capId),
      tx.object(ctx.accountId),
      tx.pure.id(p.receiptId),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
}
