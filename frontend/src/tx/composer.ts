// Programmable Transaction Block builders for the Earn (writer) and Buy
// (trader) composers.
//
// Shapes mirror the split Move signatures in `contracts/sources/bucket.move`:
//   bucket::execute_write_writer_flow<U, S, Call>(bucket, org, config,
//     treasury, signer_account, underlying_in, signed_quote, clock, ctx)
//       -> (Position, Coin<Settlement>)
//   bucket::execute_write_trader_flow<U, S, Call>(bucket, org, config,
//     treasury, signer_account, premium_in, signed_quote, clock, ctx)
//       -> Coin<Call>
//
// The contract RETURNS the executor's side to the PTB; we transfer it to the
// connected wallet with a trailing `transferObjects`. The signer/MM's side
// is still routed by the contract to the quote's `signer_token_recipient`.
//
// `Quote` / `SignedQuote` are Move structs, not pure args, so we rebuild
// them on chain from the MM's signed RFQ entry via `quote::new_quote` +
// `quote::new_signed_quote`. The struct must BCS-encode to the exact bytes
// the MM signed, so every field is reconstructed verbatim from the quote.
//
// NOTE: the gas-station only sponsors PTBs that match its templates
// (`rust-backend/crates/sui-tx/src/tx/template.rs`). Any change to the
// shapes built here MUST update those templates in the same change set —
// see `.claude/ptb-sync.md`.

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
  /** The bucket's Org (shared object; org fee is deposited into it). */
  orgId: string;
  /** `series.asset_coin_type` — the `Underlying` type arg. */
  underlyingCoinType: string;
  /** `series.settlement_coin_type` — the `Settlement` type arg. */
  settlementCoinType: string;
  /** The bucket's per-bucket option coin type (`Call` type arg). */
  callCoinType: string;
  /** Connected wallet; receives the returned Position NFT and net premium. */
  writer: string;
};

/**
 * Build a writer-flow `execute_write_writer_flow` PTB from a signed RFQ
 * quote.
 *
 * The signer's `Account` (`signer_account_id`) is a shared object
 * (`account::create_and_share_account` → `transfer::share_object`), so
 * `tx.object(...)` resolves its shared metadata via dapp-kit's SuiClient,
 * the same way the bucket / org / config / treasury args do elsewhere.
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

  // Writer supplies exactly write_amount of underlying; the MM's premium is
  // debited from their Account inside the contract.
  const underlying = tx.add(
    coinWithBalance({
      balance: BigInt(q.write_amount),
      type: p.underlyingCoinType,
    }),
  );

  const [position, netPremium] = tx.moveCall({
    target: `${pkg}::bucket::execute_write_writer_flow`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.callCoinType],
    arguments: [
      tx.object(q.bucket_id),
      tx.object(p.orgId),
      tx.object(PROTOCOL_CONFIG_ID),
      tx.object(TREASURY_ID),
      tx.object(q.signer_account_id), // MM Account (shared, mutable)
      underlying,
      signedQuote,
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });

  // Returned (Position, net premium) → the writer. The MM's Coin<Call> is
  // contract-routed to the quote's signer_token_recipient.
  tx.transferObjects([position, netPremium], p.writer);

  return tx;
}

export type BuyParams = {
  /** Chosen MM quote (default: the best, `quotes[0]`). */
  entry: RfqQuoteEntry;
  /** The bucket's Org (shared object; org fee is deposited into it). */
  orgId: string;
  /** `series.asset_coin_type` — the `Underlying` type arg. */
  underlyingCoinType: string;
  /** `series.settlement_coin_type` — the `Settlement` type arg. */
  settlementCoinType: string;
  /** The bucket's per-bucket option coin type (`Call` type arg). */
  callCoinType: string;
  /** Connected wallet; pays the premium and receives the returned CallOption. */
  trader: string;
};

/**
 * Build a trader-flow `execute_write_trader_flow` PTB from a signed RFQ
 * quote.
 *
 * Mirror of {@link buildWriteTx} for the Buy page. The Writer MM (signer)
 * supplies the underlying from their Account and is contract-routed the
 * Position NFT; the trader pays the premium from their wallet and receives
 * the returned `Coin<Call>` via the trailing transfer.
 */
export function buildBuyTx(p: BuyParams): Transaction {
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

  // Trader pays exactly the premium in settlement; the MM's underlying is
  // debited from their Account inside the contract.
  const premium = tx.add(
    coinWithBalance({
      balance: BigInt(q.premium),
      type: p.settlementCoinType,
    }),
  );

  const [callCoin] = tx.moveCall({
    target: `${pkg}::bucket::execute_write_trader_flow`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.callCoinType],
    arguments: [
      tx.object(q.bucket_id),
      tx.object(p.orgId),
      tx.object(PROTOCOL_CONFIG_ID),
      tx.object(TREASURY_ID),
      tx.object(q.signer_account_id), // MM Account (shared, mutable)
      premium,
      signedQuote,
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });

  // Returned Coin<Call> → the trader. The MM's Position is contract-routed
  // to the quote's signer_token_recipient.
  tx.transferObjects([callCoin], p.trader);

  return tx;
}
