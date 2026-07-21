// Programmable Transaction Block builders for the curated trading vault's
// wallet-facing flows (SO-288).
//
// Shapes mirror the Move signatures in
// `contracts/trading-vault/sources/vault.move`. Each call takes the vault's
// single deposit-asset type arg `<T>` where the signature is generic;
// `request_withdraw` takes none (shares is a plain u128).

import { Transaction, coinWithBalance } from "@mysten/sui/transactions";

import {
  DEEPBOOK_ADAPTER_PACKAGE_ID,
  ENV,
  TRADING_VAULT_OBJECTS,
  TRADING_VAULT_PACKAGE_ID,
} from "../config";

const CLOCK_ID = "0x6";

function requirePackage(): string {
  if (!TRADING_VAULT_PACKAGE_ID) {
    throw new Error(
      `No trading-vault deployment for VITE_ENVIRONMENT="${ENV}" (token-info returned no tradingVault packageId) — cannot build trading-vault PTBs`,
    );
  }
  return TRADING_VAULT_PACKAGE_ID;
}

export type CreateTradingVaultParams = {
  /** Shared `VaultProtocolConfig` object id (resolved from the publish tx). */
  protocolConfigId: string;
  /** The vault's deposit-asset coin type — the `T` type arg. */
  depositCoinType: string;
  curator: string;
  lockupMs: number;
  curatorFeeBps: number;
  /** 0 = creator, 1 = curator, 2 = either. */
  rotationAuthority: number;
  maxPositions: number;
  unwindGraceMs: number;
};

/**
 * `vault::create_vault<T>` — permissionless vault creation. The `CuratorCap`
 * is transferred to the curator inside the call; the vault is shared.
 */
export function buildCreateTradingVaultTx(p: CreateTradingVaultParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::vault::create_vault`,
    typeArguments: [p.depositCoinType],
    arguments: [
      tx.object(p.protocolConfigId),
      tx.pure.address(p.curator),
      tx.pure.u64(p.lockupMs),
      tx.pure.u64(p.curatorFeeBps),
      tx.pure.u8(p.rotationAuthority),
      tx.pure.u64(p.maxPositions),
      tx.pure.u64(p.unwindGraceMs),
    ],
  });
  return tx;
}

export type TradingVaultDepositParams = {
  vaultId: string;
  /** Shared `VaultProtocolConfig` object id. */
  protocolConfigId: string;
  depositCoinType: string;
  /** Deposit amount in smallest units. */
  amountRaw: bigint;
};

/**
 * `vault::begin_appraisal<T>` piped into `vault::deposit<T>` — mint shares at
 * NAV into the sender's stake. This two-call PTB only succeeds while the vault
 * holds nothing but its deposit asset (no positions, no other balances);
 * otherwise the appraisal is incomplete and `deposit` aborts. Appraisal legs
 * for held positions are a follow-up.
 */
export function buildTradingVaultDepositTx(p: TradingVaultDepositParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  const appraisal = tx.moveCall({
    target: `${pkg}::vault::begin_appraisal`,
    typeArguments: [p.depositCoinType],
    arguments: [tx.object(p.vaultId)],
  });
  const funds = tx.add(
    coinWithBalance({ balance: p.amountRaw, type: p.depositCoinType }),
  );
  tx.moveCall({
    target: `${pkg}::vault::deposit`,
    typeArguments: [p.depositCoinType],
    arguments: [
      tx.object(p.vaultId),
      tx.object(p.protocolConfigId),
      appraisal,
      funds,
      tx.object(CLOCK_ID),
    ],
  });
  return tx;
}

export type TradingVaultWithdrawParams = {
  vaultId: string;
  /** Shares to queue, in atomic share units (u128 — NOT u64). */
  sharesRaw: bigint;
};

/**
 * `vault::request_withdraw` — queue a FIFO withdrawal from the sender's stake.
 * No type args; `shares` is a u128, so it must be encoded with `pure.u128`.
 */
export function buildTradingVaultWithdrawTx(p: TradingVaultWithdrawParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::vault::request_withdraw`,
    arguments: [
      tx.object(p.vaultId),
      tx.pure.u128(p.sharesRaw),
      tx.object(CLOCK_ID),
    ],
  });
  return tx;
}

export type CuratorTakerSwapParams = {
  vaultId: string;
  /** The curator's owned `CuratorCap` object id. */
  curatorCapId: string;
  /** Allowlisted DeepBook `Pool<B, Q>` object id + its type args. */
  poolId: string;
  baseType: string;
  quoteType: string;
  /** true = sell Base for Quote, false = buy Base with Quote. */
  baseForQuote: boolean;
  /** Input amount in the input asset's smallest units. */
  amountRaw: bigint;
  /** Minimum acceptable output in the output asset's smallest units. */
  minOutRaw: bigint;
};

/**
 * `deepbook_adapter::taker_swap_base_for_quote` / `taker_swap_quote_for_base`
 * (SO-299 spot tab): a curator taker swap of vault FREE balances against an
 * allowlisted pool — a single session-gated call, no custody or appraisal
 * needed. The custody surface (`init_custody` → BM deposits → resting
 * limit/market orders) is deferred: it needs custody discovery, order
 * management, and per-pool book UX well beyond a first spot tab.
 *
 * Curator ops are NOT gas-sponsored (sui-tx template.rs) — submit wallet-paid.
 */
export function buildCuratorTakerSwapTx(p: CuratorTakerSwapParams): Transaction {
  const pkg = DEEPBOOK_ADAPTER_PACKAGE_ID;
  if (!pkg) throw new Error("deepbook-adapter package not deployed on this network");
  const gov = TRADING_VAULT_OBJECTS;
  if (!gov) throw new Error("trading-vault governance objects not served by token-info");
  const fn = p.baseForQuote ? "taker_swap_base_for_quote" : "taker_swap_quote_for_base";
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::deepbook_adapter::${fn}`,
    typeArguments: [p.baseType, p.quoteType],
    arguments: [
      tx.object(p.vaultId),
      tx.object(p.curatorCapId),
      tx.object(gov.integrationRegistryId),
      tx.object(gov.poolAllowlistId),
      tx.object(p.poolId),
      tx.pure.u64(p.amountRaw),
      tx.pure.u64(p.minOutRaw),
      tx.object(CLOCK_ID),
    ],
  });
  return tx;
}
