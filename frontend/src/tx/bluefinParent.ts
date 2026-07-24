// Parent-address transaction builders (SO-305): the two Sui transactions
// the FROST parent account itself sends, each co-signed by the ceremony
// (hedge-signer payload kind `sui_tx`):
//
// 1. `exchange::deposit_to_asset_bank` — materialize/fund the Bluefin
//    account with collateral the vault released to the parent address
//    (policy: single deposit call, pinned package + eds, target = parent);
// 2. `vault::return_external<T>` — the sweep: pay withdrawn funds back into
//    the vault (policy: strict tier, every output pays the vault).
//
// Both build FULL TransactionData bytes (sender = parent, gas selected from
// the parent's SUI) — the exact bytes the ceremony signs and gRPC executes.

import { Transaction, coinWithBalance } from "@mysten/sui/transactions";
import type { SuiGrpcClient } from "@mysten/sui/grpc";

import { TRADING_VAULT_PACKAGE_ID } from "../config";

export type ParentDepositParams = {
  client: SuiGrpcClient;
  parentAddress: string;
  /** Bluefin CURRENT package (exchange-info `contractsConfig.currentContractAddress`). */
  bluefinPackageId: string;
  /** Shared ExternalDataStore (`contractsConfig.edsId`). */
  edsId: string;
  /** Venue collateral symbol + coin type (exchange-info `assets`). */
  assetSymbol: string;
  coinType: string;
  /** Amount in the coin's NATIVE units (e.g. 6dp for USDC — the on-chain
   * param is coin units, not Bluefin's E9; python SDK docs). */
  amountRaw: bigint;
};

/** `deposit_to_asset_bank<T>(eds, asset_symbol, target_address, amount,
 * coin, ctx)` crediting the parent itself. */
export async function buildParentDepositTxBytes(p: ParentDepositParams): Promise<Uint8Array> {
  const tx = new Transaction();
  tx.setSender(p.parentAddress);
  const coin = tx.add(coinWithBalance({ balance: p.amountRaw, type: p.coinType }));
  tx.moveCall({
    target: `${p.bluefinPackageId}::exchange::deposit_to_asset_bank`,
    typeArguments: [p.coinType],
    arguments: [
      tx.object(p.edsId),
      tx.pure.string(p.assetSymbol),
      tx.pure.address(p.parentAddress),
      tx.pure.u64(p.amountRaw),
      coin,
    ],
  });
  return tx.build({ client: p.client });
}

export type ParentSweepParams = {
  client: SuiGrpcClient;
  parentAddress: string;
  vaultId: string;
  /** The vault's deposit-asset coin type (`return_external`'s `T`). */
  depositCoinType: string;
  /** Amount to sweep back, in deposit-asset smallest units. */
  amountRaw: bigint;
};

/** `vault::return_external<T>(vault, funds, ctx)` — accepted on-chain only
 * because the sender IS the registered external account; reduces the
 * vault's outstanding exposure by the coin's value. */
export async function buildParentSweepTxBytes(p: ParentSweepParams): Promise<Uint8Array> {
  if (!TRADING_VAULT_PACKAGE_ID) {
    throw new Error("trading-vault package is not deployed on this network");
  }
  const tx = new Transaction();
  tx.setSender(p.parentAddress);
  const funds = tx.add(
    coinWithBalance({ balance: p.amountRaw, type: p.depositCoinType }),
  );
  tx.moveCall({
    target: `${TRADING_VAULT_PACKAGE_ID}::vault::return_external`,
    typeArguments: [p.depositCoinType],
    arguments: [tx.object(p.vaultId), funds],
  });
  return tx.build({ client: p.client });
}

/** Execute ceremony-signed parent bytes via gRPC (same digest semantics as
 * tx/submit.ts: a FailedTransaction still has a digest). */
export async function executeParentTx(
  client: SuiGrpcClient,
  txBytes: Uint8Array,
  suiSignatureB64: string,
): Promise<string> {
  const res = await client.core.executeTransaction({
    transaction: txBytes,
    signatures: [suiSignatureB64],
  });
  return (res.Transaction ?? res.FailedTransaction).digest;
}
