// Permissionless option-market listing (SO-415/416).
//
// `exchange_listing::create_call_market<U, S, D0..D9>(auth, bucket, clock)`
// (and the `_put_` twin) lists an exchange market for an existing bucket —
// the type args are exactly the bucket's option coin's 12 type parameters,
// in order, parsed out of the coin type string the api-service serves
// (contracts/exchange-listing/sources/exchange_listing.move).

import { Transaction } from "@mysten/sui/transactions";
import { SUI_CLOCK_OBJECT_ID } from "@mysten/sui/utils";

import {
  ENV,
  EXCHANGE_LISTING_AUTHORITY_ID,
  EXCHANGE_LISTING_PACKAGE_ID,
} from "../config";

/** Whether this deployment supports listing (token-info serves the block /
 * a VITE override points at one). Gates the "List market" UI. */
export function canListMarkets(): boolean {
  return Boolean(EXCHANGE_LISTING_PACKAGE_ID && EXCHANGE_LISTING_AUTHORITY_ID);
}

/** Split `T<A, B<C, D>, E>`'s top-level type arguments: ["A","B<C,D>","E"]. */
export function splitTypeArgs(coinType: string): string[] {
  const open = coinType.indexOf("<");
  if (open < 0 || !coinType.endsWith(">")) return [];
  const inner = coinType.slice(open + 1, -1);
  const args: string[] = [];
  let depth = 0;
  let start = 0;
  for (let i = 0; i < inner.length; i++) {
    const c = inner[i];
    if (c === "<") depth++;
    else if (c === ">") depth--;
    else if (c === "," && depth === 0) {
      args.push(inner.slice(start, i).trim());
      start = i + 1;
    }
  }
  args.push(inner.slice(start).trim());
  return args.filter((a) => a.length > 0);
}

export type ListMarketParams = {
  /** Shared `Bucket` / `PutBucket` object id. */
  bucketId: string;
  /** The bucket's full option coin type (`…::option_coin::OptionCall<…>`). */
  optionCoinType: string;
  isPut: boolean;
};

/**
 * The sponsored list-market PTB. Type arguments are the option coin's
 * `<U, S, D0..D9>` in order; the quote side is structurally forced to the
 * bucket's settlement coin `S` on-chain.
 */
export function buildListMarketTx(p: ListMarketParams): Transaction {
  if (!EXCHANGE_LISTING_PACKAGE_ID || !EXCHANGE_LISTING_AUTHORITY_ID) {
    throw new Error(
      `No exchange-listing deployment for VITE_ENVIRONMENT="${ENV}" (token-info returned no exchangeListing block)`,
    );
  }
  const typeArguments = splitTypeArgs(p.optionCoinType);
  if (typeArguments.length !== 12) {
    throw new Error(
      `option coin type has ${typeArguments.length} type args, expected 12 (U, S, D0..D9): ${p.optionCoinType}`,
    );
  }
  const tx = new Transaction();
  tx.moveCall({
    target: `${EXCHANGE_LISTING_PACKAGE_ID}::exchange_listing::${p.isPut ? "create_put_market" : "create_call_market"}`,
    typeArguments,
    arguments: [
      tx.object(EXCHANGE_LISTING_AUTHORITY_ID),
      tx.object(p.bucketId),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
  return tx;
}
