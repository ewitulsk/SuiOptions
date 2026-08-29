// Spoke-chain write helpers. Each prompts the injected wallet (after forcing
// it onto the configured spoke chain), submits, and waits for the receipt on
// our own RPC — returning the tx hash, mirroring how tx/submit.ts hands the
// digest back on the Sui side.

import type { Address, Hash, WalletClient } from "viem";

import { SPOKE_CONFIG, type SpokeConfig } from "../config";
import { spokeVaultAbi, usdgAbi } from "./abi";
import {
  ensureSpokeChain,
  getSpokePublicClient,
  getSpokeWalletClient,
} from "./client";

function cfg(): SpokeConfig {
  if (!SPOKE_CONFIG) throw new Error("No spoke deployment on this network");
  return SPOKE_CONFIG;
}

async function walletOnSpokeChain(): Promise<WalletClient> {
  const wallet = getSpokeWalletClient();
  await ensureSpokeChain(wallet);
  return wallet;
}

async function confirmed(hash: Hash): Promise<Hash> {
  const receipt = await getSpokePublicClient().waitForTransactionReceipt({ hash });
  if (receipt.status === "reverted") {
    throw new Error(`Transaction reverted (${hash})`);
  }
  return hash;
}

/** ERC-20 `approve(spokeVault, amount)` on the deposit asset. */
export async function approveUsdg(account: Address, amountRaw: bigint): Promise<Hash> {
  const c = cfg();
  const wallet = await walletOnSpokeChain();
  const hash = await wallet.writeContract({
    address: c.usdgAddress,
    abi: usdgAbi,
    functionName: "approve",
    args: [c.spokeVaultAddress, amountRaw],
    account,
    chain: wallet.chain,
  });
  return confirmed(hash);
}

/** `SpokeVault.deposit(assetCode, amount, tranche)` — escrows as `pending`
 * until the hub ACK flips it to `active` (plan §4). */
export async function depositUsdg(
  account: Address,
  amountRaw: bigint,
  tranche: number,
): Promise<Hash> {
  const c = cfg();
  const wallet = await walletOnSpokeChain();
  const hash = await wallet.writeContract({
    address: c.spokeVaultAddress,
    abi: spokeVaultAbi,
    functionName: "deposit",
    args: [c.assetCode, amountRaw, tranche],
    account,
    chain: wallet.chain,
  });
  return confirmed(hash);
}

/** `SpokeVault.reclaim(seq)` — refund a deposit no hub ACK reached within
 * DEPOSIT_TIMEOUT. */
export async function reclaimDeposit(account: Address, depositSeq: bigint): Promise<Hash> {
  const c = cfg();
  const wallet = await walletOnSpokeChain();
  const hash = await wallet.writeContract({
    address: c.spokeVaultAddress,
    abi: spokeVaultAbi,
    functionName: "reclaim",
    args: [depositSeq],
    account,
    chain: wallet.chain,
  });
  return confirmed(hash);
}

/** `SpokeVault.requestWithdraw(tranche, shares, all)` — share-denominated;
 * the hub prices and directs payment via WithdrawAck (plan §5). */
export async function requestWithdraw(
  account: Address,
  tranche: number,
  shares: bigint,
  all: boolean,
): Promise<Hash> {
  const c = cfg();
  const wallet = await walletOnSpokeChain();
  const hash = await wallet.writeContract({
    address: c.spokeVaultAddress,
    abi: spokeVaultAbi,
    functionName: "requestWithdraw",
    args: [tranche, shares, all],
    account,
    chain: wallet.chain,
  });
  return confirmed(hash);
}

/** Permissionless FIFO queue drain from `active` funds. */
export async function processPayoutQueue(account: Address): Promise<Hash> {
  const c = cfg();
  const wallet = await walletOnSpokeChain();
  const hash = await wallet.writeContract({
    address: c.spokeVaultAddress,
    abi: spokeVaultAbi,
    functionName: "processPayoutQueue",
    args: [c.assetCode],
    account,
    chain: wallet.chain,
  });
  return confirmed(hash);
}

/** TUSDG open faucet mint to the caller — testnet-set only. */
export async function mintTusdg(account: Address, amountRaw: bigint): Promise<Hash> {
  const c = cfg();
  const wallet = await walletOnSpokeChain();
  const hash = await wallet.writeContract({
    address: c.usdgAddress,
    abi: usdgAbi,
    functionName: "mintToSender",
    args: [amountRaw],
    account,
    chain: wallet.chain,
  });
  return confirmed(hash);
}
