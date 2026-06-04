// Programmable Transaction Block builder for the Earn (writer) composer.
//
// Shape mirrors the Move signature in `contracts/sources/bucket.move`:
//   bucket::execute_write<U, S>(bucket, config, treasury, signer_account,
//     underlying_in, premium_in, flow, position_recipient,
//     call_token_recipient, signed_quote, clock, ctx)
//
// `Quote` / `SignedQuote` are Move structs, not pure args, so we rebuild
// them on chain from the MM's signed RFQ entry via `quote::new_quote` +
// `quote::new_signed_quote`. The struct must BCS-encode to the exact bytes
// the MM signed, so every field is reconstructed verbatim from the quote.
//
// Writer-flow invariants enforced by `execute_write_with_quote`
// (FlowKind::Writer): signer_recipient == call_token_recipient;
// premium_in.value() == 0; underlying_in.value() == write_amount. The writer
// (ctx.sender()) receives the net premium and the Position NFT; the MM/buyer
// (signer_token_recipient) receives the CallOption.

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

export type WriteParams = {
  /** Chosen MM quote (default: the best, `quotes[0]`). */
  entry: RfqQuoteEntry;
  /** `series.asset_coin_type` — the `Underlying` type arg. */
  underlyingCoinType: string;
  /** `series.settlement_coin_type` — the `Settlement` type arg. */
  settlementCoinType: string;
  /** Connected wallet; receives the Position NFT and net premium. */
  writer: string;
};

/**
 * Build a writer-flow `execute_write` PTB from a signed RFQ quote.
 *
 * The signer's `Account` (`signer_account_id`) is a shared object
 * (`account::create_and_share_account` → `transfer::share_object`), so
 * `tx.object(...)` resolves its shared metadata via dapp-kit's SuiClient,
 * the same way the bucket / config / treasury args do elsewhere.
 */
export function buildWriteTx(p: WriteParams): Transaction {
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

  // Writer supplies exactly write_amount of underlying; the premium side is
  // a zero Settlement coin (the MM's premium is debited from their Account).
  const underlying = tx.add(
    coinWithBalance({
      balance: BigInt(q.write_amount),
      type: p.underlyingCoinType,
    }),
  );
  const premiumZero = tx.moveCall({
    target: "0x2::coin::zero",
    typeArguments: [p.settlementCoinType],
  });

  tx.moveCall({
    target: `${pkg}::bucket::execute_write`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType],
    arguments: [
      tx.object(q.bucket_id),
      tx.object(PROTOCOL_CONFIG_ID),
      tx.object(TREASURY_ID),
      tx.object(q.signer_account_id), // MM Account (shared, mutable)
      underlying,
      premiumZero,
      flow,
      tx.pure.address(p.writer), // position_recipient = the writer
      tx.pure.address(q.signer_token_recipient), // call_token_recipient = the MM/buyer
      signedQuote,
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });

  return tx;
}
