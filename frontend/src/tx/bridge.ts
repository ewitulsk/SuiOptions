// PTB builder for the Sui→Solana leg of the CCTP bridge.
//
// Shape (must stay in lockstep with the gas station's `cctp_bridge` template
// in `rust-backend/crates/sui-tx/src/tx/template.rs`):
//   1. coinWithBalance(USDC, amount)                    — coin plumbing
//   2. token_messenger_minter::deposit_for_burn::deposit_for_burn<USDC>
//      — Circle's entry fun: burns the coin and sends the cross-chain
//        message, with the user's own address as the message sender.
//
// We call Circle directly rather than through a wrapper package: the
// package-auth ticket flow exists so a *package* can own the message (and
// later `replace_deposit_for_burn` it), which we never do. Circle's own
// guidance is that direct/EOA callers use `deposit_for_burn`.
//
// The mint recipient is the destination wallet's Solana USDC **token
// account** (ATA), not the wallet itself, encoded as a 32-byte Sui address.

import { Transaction, coinWithBalance } from "@mysten/sui/transactions";

import type { CctpConfig } from "../api/cctpConfig";

/** Stablecoin DenyList — a fixed framework address on every network. */
const DENY_LIST = "0x403";

export type SuiDepositForBurnParams = {
  /** USDC to bridge, in base units (6 decimals). */
  amountRaw: bigint;
  /** Destination Solana USDC ATA, as 0x-prefixed 32-byte hex. */
  mintRecipientHex: string;
};

export function buildSuiDepositForBurnTx(
  cctp: CctpConfig,
  p: SuiDepositForBurnParams,
): Transaction {
  const tx = new Transaction();

  const coin = tx.add(
    coinWithBalance({ balance: p.amountRaw, type: cctp.sui.usdcCoinType }),
  );

  tx.moveCall({
    target: `${cctp.sui.tokenMessengerPackage}::deposit_for_burn::deposit_for_burn`,
    typeArguments: [cctp.sui.usdcCoinType],
    arguments: [
      coin,
      tx.pure.u32(cctp.domainSolana),
      tx.pure.address(p.mintRecipientHex),
      tx.object(cctp.sui.tokenMessengerState),
      tx.object(cctp.sui.messageTransmitterState),
      tx.object(DENY_LIST),
      tx.object(cctp.sui.usdcTreasury),
    ],
  });

  return tx;
}
