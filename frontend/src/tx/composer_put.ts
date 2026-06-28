// Programmable Transaction Block builder for cash-secured PUT writes/buys.
//
// Mirror of `tx/composer.ts` (covered calls), targeting the `put_bucket`
// module. The crucial difference: BOTH coin args of `put_bucket::execute_write`
// are `Coin<Settlement>` — a put writer posts settlement *cash* collateral
// (not the underlying), and the buyer pays the premium in settlement.
//
// Shape mirrors the Move signature in `contracts/sources/put_bucket.move`:
//   put_bucket::execute_write<U, S, Put>(bucket, config, treasury, signer_account,
//     collateral_in: Coin<Settlement>, premium_in: Coin<Settlement>, flow,
//     position_recipient, put_token_recipient, signed_quote, clock, ctx)
//
// Flow markers reuse the CALL module: `bucket::writer_flow()` /
// `bucket::trader_flow()`. The signed-quote prelude is identical to the call
// path — the struct must BCS-encode to the exact bytes the MM signed.
//
// Writer flow: executor (writer) supplies `collateral_in` =
//   ceil(write_amount * strike / 10^strike_scale) settlement, premium_in =
//   coin::zero<Settlement>. Trader flow: executor (buyer) supplies premium_in =
//   premium settlement, collateral_in = coin::zero<Settlement>.

import { Transaction, coinWithBalance } from "@mysten/sui/transactions";
import { SUI_CLOCK_OBJECT_ID, fromHex } from "@mysten/sui/utils";

import { ENV, PACKAGE_ID, PROTOCOL_CONFIG_ID, TREASURY_ID } from "../config";
import type { RfqQuoteEntry } from "../api/quoting";

function requirePackage(): string {
  if (!PACKAGE_ID) {
    throw new Error(
      `No deployment for VITE_ENVIRONMENT="${ENV}" (token-info returned no packageId) — the composer cannot build PTBs against the protocol`,
    );
  }
  return PACKAGE_ID;
}

function strip0x(s: string): string {
  return s.startsWith("0x") ? s.slice(2) : s;
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
 * Build a writer-flow `put_bucket::execute_write` PTB from a signed RFQ quote.
 *
 * The put writer posts settlement *cash* collateral
 * (ceil(write_amount*strike/10^scale)); the premium side is a zero settlement
 * coin (the MM/buyer's premium is debited from their Account).
 */
export function buildWritePutTx(p: WritePutParams): Transaction {
  const pkg = requirePackage();
  if (!PROTOCOL_CONFIG_ID || !TREASURY_ID) {
    throw new Error(
      `Missing protocolConfigId/treasuryId for VITE_ENVIRONMENT="${ENV}" — cannot build execute_write`,
    );
  }
  const q = p.entry.quote;
  const tx = new Transaction();

  // Reconstruct the signed quote on chain. Hex fields → vector<u8>.
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

  const flow = tx.moveCall({ target: `${pkg}::bucket::writer_flow` });

  // Writer supplies cash collateral = ceil(write_amount*strike/10^scale) of
  // settlement; the premium side is a zero Settlement coin.
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
  const premiumZero = tx.moveCall({
    target: "0x2::coin::zero",
    typeArguments: [p.settlementCoinType],
  });

  tx.moveCall({
    target: `${pkg}::put_bucket::execute_write`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.putCoinType],
    arguments: [
      tx.object(q.bucket_id),
      tx.object(PROTOCOL_CONFIG_ID),
      tx.object(TREASURY_ID),
      tx.object(q.signer_account_id), // MM Account (shared, mutable)
      collateral,
      premiumZero,
      flow,
      tx.pure.address(p.writer), // position_recipient = the writer
      tx.pure.address(q.signer_token_recipient), // put_token_recipient = the MM/buyer
      signedQuote,
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
 * Build a trader-flow `put_bucket::execute_write` PTB from a signed RFQ quote.
 *
 * Mirror of {@link buildWritePutTx} for the Buy page. The trader pays the
 * premium in settlement; the collateral side is a zero settlement coin (the MM
 * writer's cash collateral is debited from their Account).
 */
export function buildBuyPutTx(p: BuyPutParams): Transaction {
  const pkg = requirePackage();
  if (!PROTOCOL_CONFIG_ID || !TREASURY_ID) {
    throw new Error(
      `Missing protocolConfigId/treasuryId for VITE_ENVIRONMENT="${ENV}" — cannot build execute_write`,
    );
  }
  const q = p.entry.quote;
  const tx = new Transaction();

  // Reconstruct the signed quote on chain. Hex fields → vector<u8>.
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

  const flow = tx.moveCall({ target: `${pkg}::bucket::trader_flow` });

  // Trader pays exactly the premium in settlement; the collateral side is a
  // zero settlement coin (the MM's cash collateral is debited from their Account).
  const premium = tx.add(
    coinWithBalance({
      balance: BigInt(q.premium),
      type: p.settlementCoinType,
    }),
  );
  const collateralZero = tx.moveCall({
    target: "0x2::coin::zero",
    typeArguments: [p.settlementCoinType],
  });

  tx.moveCall({
    target: `${pkg}::put_bucket::execute_write`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.putCoinType],
    arguments: [
      tx.object(q.bucket_id),
      tx.object(PROTOCOL_CONFIG_ID),
      tx.object(TREASURY_ID),
      tx.object(q.signer_account_id), // MM Account (shared, mutable)
      collateralZero,
      premium,
      flow,
      tx.pure.address(q.signer_token_recipient), // position_recipient = the MM/writer
      tx.pure.address(p.trader), // put_token_recipient = the trader
      signedQuote,
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });

  return tx;
}
