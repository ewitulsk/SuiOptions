// Programmable Transaction Block builders for covered-call RFQ fills.
//
// Collateral-abstraction shape (contracts/core/sources/bucket.move +
// collateral.move): core mints a no-abilities `CollateralRequest` potato
// after verifying the signed quote; the MM's own collateral package
// releases funds against it; core consumes potato + funds in the same tx.
//
//   quote::new_quote → quote::new_signed_quote
//   → bucket::request_writer_flow / request_trader_flow   (mints the potato)
//   → {release_package}::{release_module}::release<T>     (MM-specified)
//   → bucket::execute_writer_flow / execute_trader_flow   (consumes both)
//
// `Quote` / `SignedQuote` are Move structs, not pure args, so we rebuild
// them on chain from the MM's signed RFQ entry via `quote::new_quote` +
// `quote::new_signed_quote`. The struct must BCS-encode to the exact bytes
// the MM signed, so every field — including the collateral routing
// (`collateral_source`, `release_package`, `release_module`) — is
// reconstructed verbatim from the quote. The routing is INSIDE the signed
// payload: a quote with bad routing simply fails to execute; it can never
// move the wrong funds.

import { Transaction, coinWithBalance } from "@mysten/sui/transactions";
import { SUI_CLOCK_OBJECT_ID, fromHex } from "@mysten/sui/utils";

import { ENV, PACKAGE_ID, PROTOCOL_CONFIG_ID, TREASURY_ID } from "../config";
import type { Quote, RfqQuoteEntry } from "../api/quoting";

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
 * Rebuild the signed quote on chain. Hex fields → vector<u8>;
 * `release_module` is a `std::string::String` (pure string). Arg order is
 * normative — it must BCS-encode to the exact bytes the MM signed.
 */
export function addSignedQuote(
  tx: Transaction,
  pkg: string,
  entry: RfqQuoteEntry,
) {
  const q = entry.quote;
  const quoteArg = tx.moveCall({
    target: `${pkg}::quote::new_quote`,
    arguments: [
      tx.pure.vector("u8", Array.from(fromHex(strip0x(q.protocol_id)))),
      tx.pure.id(q.signer_id),
      tx.pure.id(q.collateral_source),
      tx.pure.address(q.release_package),
      tx.pure.string(q.release_module),
      tx.pure.address(q.signer_token_recipient),
      tx.pure.id(q.bucket_id),
      tx.pure.u64(BigInt(q.write_amount)),
      tx.pure.u64(BigInt(q.premium)),
      tx.pure.u64(BigInt(q.valid_until_ms)),
      tx.pure.u64(BigInt(q.nonce)),
    ],
  });
  return tx.moveCall({
    target: `${pkg}::quote::new_signed_quote`,
    arguments: [
      quoteArg,
      tx.pure.vector("u8", Array.from(fromHex(strip0x(entry.signature)))),
    ],
  });
}

/**
 * The MM-specified release call: debits `collateral_source` (a shared
 * object of the MM's own collateral package) against the potato and returns
 * `Balance<T>`. Target + source come straight from the signed quote.
 */
export function addRelease(
  tx: Transaction,
  q: Quote,
  request: ReturnType<Transaction["moveCall"]>,
  coinType: string,
) {
  return tx.moveCall({
    target: `${q.release_package}::${q.release_module}::release`,
    typeArguments: [coinType],
    arguments: [tx.object(q.collateral_source), request],
  });
}

export type WriteParams = {
  /** Chosen MM quote (default: the best, `quotes[0]`). */
  entry: RfqQuoteEntry;
  /** `series.asset_coin_type` — the `Underlying` type arg. */
  underlyingCoinType: string;
  /** `series.settlement_coin_type` — the `Settlement` type arg. */
  settlementCoinType: string;
  /** The bucket's per-bucket option coin type (`Call` type arg). */
  callCoinType: string;
  /** Connected wallet; receives the Position Object and net premium. */
  writer: string;
};

/**
 * Build a writer-flow PTB from a signed RFQ quote: the retail writer
 * (ctx.sender()) supplies exactly `write_amount` of underlying and receives
 * the Position + net premium; the MM's premium is released from their
 * collateral source as `Balance<Settlement>`; the MM/buyer receives the
 * CallOption at the quote's `signer_token_recipient`.
 */
export function buildWriteTx(p: WriteParams): Transaction {
  const pkg = requirePackage();
  if (!PROTOCOL_CONFIG_ID || !TREASURY_ID) {
    throw new Error(
      `Missing protocolConfigId/treasuryId for VITE_ENVIRONMENT="${ENV}" — cannot build the write PTB`,
    );
  }
  const q = p.entry.quote;
  const tx = new Transaction();
  const typeArgs = [p.underlyingCoinType, p.settlementCoinType, p.callCoinType];

  const signedQuote = addSignedQuote(tx, pkg, p.entry);

  // Verify the quote (consuming its nonce) and mint the premium demand.
  const request = tx.moveCall({
    target: `${pkg}::bucket::request_writer_flow`,
    typeArguments: typeArgs,
    arguments: [
      tx.object(q.bucket_id),
      tx.object(q.signer_id), // MM QuoteSigner (shared, mutable)
      tx.object(PROTOCOL_CONFIG_ID),
      signedQuote,
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });

  // MM premium, released from their collateral source.
  const premiumFunds = addRelease(tx, q, request, p.settlementCoinType);

  // Writer supplies exactly write_amount of underlying.
  const underlying = tx.add(
    coinWithBalance({
      balance: BigInt(q.write_amount),
      type: p.underlyingCoinType,
    }),
  );

  tx.moveCall({
    target: `${pkg}::bucket::execute_writer_flow`,
    typeArguments: typeArgs,
    arguments: [
      tx.object(q.bucket_id),
      tx.object(PROTOCOL_CONFIG_ID),
      tx.object(TREASURY_ID),
      request,
      premiumFunds,
      underlying,
      tx.pure.address(p.writer), // position_recipient = the writer
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });

  return tx;
}

export type BuyParams = {
  /** Chosen MM quote (default: the best, `quotes[0]`). */
  entry: RfqQuoteEntry;
  /** `series.asset_coin_type` — the `Underlying` type arg. */
  underlyingCoinType: string;
  /** `series.settlement_coin_type` — the `Settlement` type arg. */
  settlementCoinType: string;
  /** The bucket's per-bucket option coin type (`Call` type arg). */
  callCoinType: string;
  /** Connected wallet; pays the premium and receives the CallOption. */
  trader: string;
};

/**
 * Build a trader-flow PTB from a signed RFQ quote. Mirror of
 * {@link buildWriteTx} for the Buy page: the writer MM's underlying is
 * released from their collateral source as `Balance<Underlying>`; the
 * trader (ctx.sender()) pays the premium from their wallet and receives
 * the CallOption; the MM receives the Position + net premium at
 * `signer_token_recipient`.
 */
export function buildBuyTx(p: BuyParams): Transaction {
  const pkg = requirePackage();
  if (!PROTOCOL_CONFIG_ID || !TREASURY_ID) {
    throw new Error(
      `Missing protocolConfigId/treasuryId for VITE_ENVIRONMENT="${ENV}" — cannot build the buy PTB`,
    );
  }
  const q = p.entry.quote;
  const tx = new Transaction();
  const typeArgs = [p.underlyingCoinType, p.settlementCoinType, p.callCoinType];

  const signedQuote = addSignedQuote(tx, pkg, p.entry);

  // Verify the quote (consuming its nonce) and mint the underlying demand.
  const request = tx.moveCall({
    target: `${pkg}::bucket::request_trader_flow`,
    typeArguments: typeArgs,
    arguments: [
      tx.object(q.bucket_id),
      tx.object(q.signer_id), // MM QuoteSigner (shared, mutable)
      tx.object(PROTOCOL_CONFIG_ID),
      signedQuote,
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });

  // MM write collateral (the underlying), released from their source.
  const underlyingFunds = addRelease(tx, q, request, p.underlyingCoinType);

  // Trader pays exactly the premium in settlement.
  const premium = tx.add(
    coinWithBalance({
      balance: BigInt(q.premium),
      type: p.settlementCoinType,
    }),
  );

  tx.moveCall({
    target: `${pkg}::bucket::execute_trader_flow`,
    typeArguments: typeArgs,
    arguments: [
      tx.object(q.bucket_id),
      tx.object(PROTOCOL_CONFIG_ID),
      tx.object(TREASURY_ID),
      request,
      underlyingFunds,
      premium,
      tx.pure.address(p.trader), // call_token_recipient = the trader
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });

  return tx;
}
