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
import { addCreateBucket, addShareBucket } from "./anystrike";
import type { Quote, RfqQuoteEntry } from "../api/quoting";
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

/**
 * The bucket the PTB writes into, and the create/share legs when it does not
 * exist yet.
 *
 * A listed strike is a strike nobody has created — the board advertises a
 * ladder, and a bucket becomes an object the first time someone writes at it.
 * So the fill IS the creation: `create_*_any_strike` is prepended and the
 * mandatory `share_*` appended, both permitted by the gas-station write
 * template, which is why this needs no sponsorship change.
 */
function addBucketArg(
  tx: Transaction,
  q: Quote,
  p: {
    bucketId: string | null;
    underlyingCoinType: string;
    settlementCoinType: string;
    coinDecimals: number;
  },
): {
  bucket: ReturnType<Transaction["moveCall"]> | ReturnType<Transaction["object"]>;
  created: { coinType: string } | null;
} {
  if (p.bucketId !== null) {
    return { bucket: tx.object(p.bucketId), created: null };
  }
  const { bucket, coinType } = addCreateBucket(tx, {
    underlyingCoinType: p.underlyingCoinType,
    settlementCoinType: p.settlementCoinType,
    expiryMs: Number(q.spec.expiry_ms),
    // The spec carries the NORMALIZED strike, and creation re-normalizes, so
    // (sig, exp) is a valid raw (strike, scale) pair for the same bucket.
    strikeRaw: BigInt(q.spec.sig),
    strikeScale: q.spec.exp,
    coinDecimals: p.coinDecimals,
    isPut: q.spec.is_put,
  });
  return { bucket, created: { coinType } };
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
  /** The bucket's object id, or null when this transaction creates it. */
  bucketId: string | null;
  /** Option-coin display decimals for the create leg — the underlying's. */
  coinDecimals: number;
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

  // Create-if-absent FIRST: the write template permits the create leg only
  // as a prefix, and the bucket argument has to exist before the request.
  const { bucket: bucketArg, created } = addBucketArg(tx, q, p);
  const signedQuote = addSignedQuote(tx, pkg, p.entry, p.underlyingCoinType, p.settlementCoinType);

  // Verify the quote (consuming its nonce) and mint the premium demand.
  const request = tx.moveCall({
    target: `${pkg}::put_bucket::request_writer_flow`,
    typeArguments: typeArgs,
    arguments: [
      bucketArg,
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
      bucketArg,
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

  if (created !== null) {
    // Terminal command of any transaction that creates a bucket.
    addShareBucket(tx, { ...p, isPut: q.spec.is_put }, bucketArg as never, created.coinType);
  }

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
  /** The bucket's object id, or null when this transaction creates it. */
  bucketId: string | null;
  /** Option-coin display decimals for the create leg — the underlying's. */
  coinDecimals: number;
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

  // Create-if-absent FIRST: the write template permits the create leg only
  // as a prefix, and the bucket argument has to exist before the request.
  const { bucket: bucketArg, created } = addBucketArg(tx, q, p);
  const signedQuote = addSignedQuote(tx, pkg, p.entry, p.underlyingCoinType, p.settlementCoinType);

  // Verify the quote (consuming its nonce) and mint the cash-collateral
  // demand (required_collateral(bucket, write_amount)).
  const request = tx.moveCall({
    target: `${pkg}::put_bucket::request_trader_flow`,
    typeArguments: typeArgs,
    arguments: [
      bucketArg,
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
      bucketArg,
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

  if (created !== null) {
    // Terminal command of any transaction that creates a bucket.
    addShareBucket(tx, { ...p, isPut: q.spec.is_put }, bucketArg as never, created.coinType);
  }

  return tx;
}
