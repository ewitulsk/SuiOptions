// PTB builder for the testnet faucet (SO-93).
//
// Each test token (TBTC/TUSDC/TDEEP/TWAL) is a shared `Faucet` with a
// public entry that mints straight to the caller:
//   {module}::mint_to_sender(faucet: &mut Faucet, amount: u64, ctx)
// (test-tokens/sources/{tbtc,tusdc,tdeep,twal}.move)

import { Transaction } from "@mysten/sui/transactions";

export type MintParams = {
  /** Test-tokens package id (`TEST_TOKENS[].packageId`). */
  testTokenPackageId: string;
  /** Module name, e.g. `tbtc`. */
  module: string;
  /** Shared `Faucet` object id. */
  faucetId: string;
  /** Amount in the token's smallest units. */
  amountRaw: bigint;
};

export function buildMintTx(p: MintParams): Transaction {
  const tx = new Transaction();
  tx.moveCall({
    target: `${p.testTokenPackageId}::${p.module}::mint_to_sender`,
    arguments: [tx.object(p.faucetId), tx.pure.u64(p.amountRaw)],
  });
  return tx;
}
