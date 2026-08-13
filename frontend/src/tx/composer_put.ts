// Programmable Transaction Block builders for cash-secured PUT RFQ fills.
//
// Mirror of `tx/composer.ts` (covered calls), targeting the `put_bucket`
// module. The crucial difference: BOTH collateral demands are settlement
// cash — the put writer posts ceil(write_amount × strike / 10^strike_scale)
// settlement (not the underlying), and both `release` calls are
// `release<Settlement>`.
//
// Shape (contracts/core/sources/put_bucket.move):
//   quote::new_quote → quote::new_signed_quote
//   → put_bucket::request_writer_flow / request_trader_flow   (potato)
//   → {release_package}::{release_module}::release<Settlement>
//   → put_bucket::execute_writer_flow / execute_trader_flow
//
// The signed-quote prelude is identical to the call path — the struct must
// BCS-encode to the exact bytes the MM signed, collateral routing included.

import { Transaction, coinWithBalance } from "@mysten/sui/transactions";
import { SUI_CLOCK_OBJECT_ID } from "@mysten/sui/utils";

import { ENV, PACKAGE_ID, PROTOCOL_CONFIG_ID, TREASURY_ID, WHITELIST_ID } from "../config";
import type { RfqQuoteEntry } from "../api/quoting";
import { addRelease, addSignedQuote } from "./composer";

function requirePackage(): string {
  if (!PACKAGE_ID) {
    throw new Error(
      `No deployment for VITE_ENVIRONMENT="${ENV}" (token-info returned no packageId) — the composer cannot build PTBs against the protocol`,
    );
  }
  return PACKAGE_ID;
}

/**
 * Cash collateral a put writer posts, in settlement smallest-units:
 *   ceil(write_amount * strike_raw / 10^strike_scale).
 * Ceil division on bigints so the on-chain collateral check never under-funds.
 */
function putCollateralRaw(
  writeAmountRaw: bigint,
  strikeRaw: bigint,
  strikeScale: number,
): bigint {
  const denom = BigInt(10) ** BigInt(strikeScale);
  return (writeAmountRaw * strikeRaw + denom - 1n) / denom;
}

export type WritePutParams = {
  /** Chosen MM quote (default: the best, `quotes[0]`). */
  entry: RfqQuoteEntry;
  /** `series.asset_coin_type` — the `Underlying` type arg. */
  underlyingCoinType: string;
  /** `series.settlement_coin_type` — the `Settlement` type arg. */
  settlementCoinType: string;
  /** The bucket's per-bucket option coin type (`Put` type arg). */
  putCoinType: string;
  /** Bucket `strike_raw` — for the cash-collateral computation. */
  strikeRaw: string;
  /** Bucket `strike_scale`. */
  strikeScale: number;
  /** Connected wallet; receives the Position Object and net premium. */
  writer: string;
};

/**
 * Build a writer-flow put PTB from a signed RFQ quote: the retail put
 * writer posts settlement *cash* collateral
 * (ceil(write_amount*strike/10^scale)) and receives the Position + net
 * premium; the MM's premium is released from their collateral source; the
 * MM/buyer receives the PutOption at `signer_token_recipient`.
 */
export function buildWritePutTx(p: WritePutParams): Transaction {
  const pkg = requirePackage();
  if (!PROTOCOL_CONFIG_ID || !TREASURY_ID || !WHITELIST_ID) {
    throw new Error(
      `Missing protocolConfigId/treasuryId/whitelistId for VITE_ENVIRONMENT="${ENV}" — cannot build the put write PTB`,
    );
  }
  const q = p.entry.quote;
  const tx = new Transaction();
  const typeArgs = [p.underlyingCoinType, p.settlementCoinType, p.putCoinType];

  const signedQuote = addSignedQuote(tx, pkg, p.entry);

  // Verify the quote (consuming its nonce) and mint the premium demand.
  const request = tx.moveCall({
    target: `${pkg}::put_bucket::request_writer_flow`,
    typeArguments: typeArgs,
    arguments: [
      tx.object(q.bucket_id),
      tx.object(q.signer_id), // MM QuoteSigner (shared, mutable)
      tx.object(PROTOCOL_CONFIG_ID),
      signedQuote,
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });

  // MM premium (settlement), released from their collateral source.
  const premiumFunds = addRelease(tx, q, request, p.settlementCoinType);

  // Writer supplies cash collateral = ceil(write_amount*strike/10^scale)
  // of settlement.
  const collateral = tx.add(
    coinWithBalance({
      balance: putCollateralRaw(
        BigInt(q.write_amount),
        BigInt(p.strikeRaw),
        p.strikeScale,
      ),
      type: p.settlementCoinType,
    }),
  );

  tx.moveCall({
    target: `${pkg}::put_bucket::execute_writer_flow`,
    typeArguments: typeArgs,
    arguments: [
      tx.object(q.bucket_id),
      tx.object(PROTOCOL_CONFIG_ID),
      tx.object(WHITELIST_ID),
      tx.object(TREASURY_ID),
      request,
      premiumFunds,
      collateral,
      tx.pure.address(p.writer), // position_recipient = the writer
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });

  return tx;
}

export type BuyPutParams = {
  /** Chosen MM quote (default: the best, `quotes[0]`). */
  entry: RfqQuoteEntry;
  /** `series.asset_coin_type` — the `Underlying` type arg. */
  underlyingCoinType: string;
  /** `series.settlement_coin_type` — the `Settlement` type arg. */
  settlementCoinType: string;
  /** The bucket's per-bucket option coin type (`Put` type arg). */
  putCoinType: string;
  /** Connected wallet; pays the premium and receives the PutOption. */
  trader: string;
};

/**
 * Build a trader-flow put PTB from a signed RFQ quote. Mirror of
 * {@link buildWritePutTx} for the Buy page: the writer MM's cash write
 * collateral (`required_collateral`, computed on chain by
 * `request_trader_flow`) is released from their collateral source; the
 * trader pays the premium in settlement and receives the PutOption; the MM
 * receives the Position + net premium at `signer_token_recipient`.
 */
export function buildBuyPutTx(p: BuyPutParams): Transaction {
  const pkg = requirePackage();
  if (!PROTOCOL_CONFIG_ID || !TREASURY_ID || !WHITELIST_ID) {
    throw new Error(
      `Missing protocolConfigId/treasuryId/whitelistId for VITE_ENVIRONMENT="${ENV}" — cannot build the put buy PTB`,
    );
  }
  const q = p.entry.quote;
  const tx = new Transaction();
  const typeArgs = [p.underlyingCoinType, p.settlementCoinType, p.putCoinType];

  const signedQuote = addSignedQuote(tx, pkg, p.entry);

  // Verify the quote (consuming its nonce) and mint the cash-collateral
  // demand (required_collateral(bucket, write_amount)).
  const request = tx.moveCall({
    target: `${pkg}::put_bucket::request_trader_flow`,
    typeArguments: typeArgs,
    arguments: [
      tx.object(q.bucket_id),
      tx.object(q.signer_id), // MM QuoteSigner (shared, mutable)
      tx.object(PROTOCOL_CONFIG_ID),
      signedQuote,
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });

  // MM cash write collateral (settlement), released from their source.
  const collateralFunds = addRelease(tx, q, request, p.settlementCoinType);

  // Trader pays exactly the premium in settlement.
  const premium = tx.add(
    coinWithBalance({
      balance: BigInt(q.premium),
      type: p.settlementCoinType,
    }),
  );

  tx.moveCall({
    target: `${pkg}::put_bucket::execute_trader_flow`,
    typeArguments: typeArgs,
    arguments: [
      tx.object(q.bucket_id),
      tx.object(PROTOCOL_CONFIG_ID),
      tx.object(WHITELIST_ID),
      tx.object(TREASURY_ID),
      request,
      collateralFunds,
      premium,
      tx.pure.address(p.trader), // put_token_recipient = the trader
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });

  return tx;
}
