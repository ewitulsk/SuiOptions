// PTB builder for the Sui→Solana leg of the CCTP bridge.
//
// Shape (must stay in lockstep with the gas station's `cctp_bridge` template
// in `rust-backend/crates/sui-tx/src/tx/template.rs`):
//   1. coinWithBalance(USDC, amount)                        — coin plumbing
//   2. cctp_bridge::bridge::prepare_deposit_for_burn<USDC>  — our entry point
//      (emits BridgeInitiated, returns the burn ticket)
//   3. Circle token_messenger_minter::deposit_for_burn::
//      deposit_for_burn_with_package_auth<USDC, BridgeAuth> — burns the coin
//      and sends the cross-chain message.
//
// The mint recipient is the destination wallet's Solana USDC **token
// account** (ATA), not the wallet itself, encoded as a 32-byte Sui address.

import { Transaction, coinWithBalance } from "@mysten/sui/transactions";

import { CCTP, CCTP_BRIDGE_PACKAGE_ID, ENV } from "../config";

function requireBridgePackage(): string {
  if (!CCTP_BRIDGE_PACKAGE_ID) {
    throw new Error(
      `No cctp_bridge deployment for VITE_ENVIRONMENT="${ENV}" — cannot build bridge PTBs`,
    );
  }
  return CCTP_BRIDGE_PACKAGE_ID;
}

export type SuiDepositForBurnParams = {
  /** USDC to bridge, in base units (6 decimals). */
  amountRaw: bigint;
  /** Destination Solana USDC ATA, as 0x-prefixed 32-byte hex. */
  mintRecipientHex: string;
};

export function buildSuiDepositForBurnTx(p: SuiDepositForBurnParams): Transaction {
  const bridge = requireBridgePackage();
  const tx = new Transaction();

  const coin = tx.add(
    coinWithBalance({ balance: p.amountRaw, type: CCTP.suiUsdcCoinType }),
  );

  const ticket = tx.moveCall({
    target: `${bridge}::bridge::prepare_deposit_for_burn`,
    typeArguments: [CCTP.suiUsdcCoinType],
    arguments: [
      coin,
      tx.pure.u32(CCTP.domainSolana),
      tx.pure.address(p.mintRecipientHex),
    ],
  });

  tx.moveCall({
    target: `${CCTP.suiTokenMessengerPackage}::deposit_for_burn::deposit_for_burn_with_package_auth`,
    typeArguments: [CCTP.suiUsdcCoinType, `${bridge}::bridge::BridgeAuth`],
    arguments: [
      ticket,
      tx.object(CCTP.suiTokenMessengerState),
      tx.object(CCTP.suiMessageTransmitterState),
      tx.object("0x403"), // stablecoin DenyList (fixed address)
      tx.object(CCTP.suiUsdcTreasury),
    ],
  });

  return tx;
}
