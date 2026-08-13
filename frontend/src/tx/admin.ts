// Programmable Transaction Block builders for the admin page.
//
// Shapes mirror the Move signatures in:
//   contracts/sources/admin.move
//     - admin::set_fee_bps(&AdminCap, &mut ProtocolConfig, new_bps)
//   contracts/sources/bucket.move
//     - bucket::invalidate_bucket<U, S, Call>(&AdminCap, &mut Bucket, reason, &Clock, ctx)
//     - bucket::revalidate_bucket<U, S, Call>(&AdminCap, &mut Bucket, reason, &Clock, ctx)
//     - bucket::cleanup_bucket<U, S, Call>(&AdminCap, Bucket, &Clock)
//   contracts/sources/treasury.move
//     - treasury::withdraw<T>(&AdminCap, &mut Treasury, amount, recipient, ctx)
//     - treasury::create_and_share(&AdminCap, ctx)
//   contracts/sources/vault.move
//     - vault::pause_deposits<U, S, V>(&AdminCap, &mut Vault)
//     - vault::unpause_deposits<U, S, V>(&AdminCap, &mut Vault)
//   contracts/whitelist/sources/whitelist.move (standalone package)
//     - add_member / remove_member / set_whitelist_enabled / set_ingress_paused
//       (&whitelist AdminCap, &mut Whitelist, value) — the ONE ingress list
//   contracts/trading-vault/sources/registry.move
//     - registry::set_paused(&core AdminCap, &mut VaultProtocolConfig, bool)
//   contracts/exchange/sources/registry.move
//     - registry::set_paused<Base, Quote>(&exchange AdminCap, &mut reg, bool)
//
// Every admin call passes the caller's owned `AdminCap` object id (see
// `useAdminCap`) as its authorizing argument. Shared objects (Bucket,
// ProtocolConfig, Treasury) are passed by id; dapp-kit's SuiClient
// resolves their shared metadata.

import { Transaction } from "@mysten/sui/transactions";
import type { TransactionArgument } from "@mysten/sui/transactions";
import { SUI_CLOCK_OBJECT_ID } from "@mysten/sui/utils";

import {
  ENV,
  EXCHANGE_PACKAGE_ID,
  PACKAGE_ID,
  TRADING_VAULT_PACKAGE_ID,
  VAULT_PACKAGE_ID,
  WHITELIST_PACKAGE_ID,
} from "../config";

function requirePackage(): string {
  if (!PACKAGE_ID) {
    throw new Error(
      `No deployment for VITE_ENVIRONMENT="${ENV}" (token-info returned no packageId) — the admin page cannot build PTBs against the protocol`,
    );
  }
  return PACKAGE_ID;
}

function requireVaultPackage(): string {
  if (!VAULT_PACKAGE_ID) {
    throw new Error(
      `No vault deployment for VITE_ENVIRONMENT="${ENV}" (token-info returned no vault packageId) — the admin page cannot build vault PTBs`,
    );
  }
  return VAULT_PACKAGE_ID;
}

/** `reason: vector<u8>` arg — UTF-8 bytes of the admin's note. */
function reasonBytes(reason: string): number[] {
  return Array.from(new TextEncoder().encode(reason));
}

export type BucketGateParams = {
  adminCapId: string;
  bucketId: string;
  underlyingCoinType: string;
  settlementCoinType: string;
  /** The bucket's per-bucket option coin type (`Call` type arg). */
  callCoinType: string;
  reason: string;
};

export function buildInvalidateBucketTx(p: BucketGateParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::bucket::invalidate_bucket`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.callCoinType],
    arguments: [
      tx.object(p.adminCapId),
      tx.object(p.bucketId),
      tx.pure.vector("u8", reasonBytes(p.reason)),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
  return tx;
}

export function buildRevalidateBucketTx(p: BucketGateParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::bucket::revalidate_bucket`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.callCoinType],
    arguments: [
      tx.object(p.adminCapId),
      tx.object(p.bucketId),
      tx.pure.vector("u8", reasonBytes(p.reason)),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
  return tx;
}

export type CleanupBucketParams = {
  adminCapId: string;
  bucketId: string;
  underlyingCoinType: string;
  settlementCoinType: string;
  /** The bucket's per-bucket option coin type (`Call` type arg). */
  callCoinType: string;
};

export function buildCleanupBucketTx(p: CleanupBucketParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  // `cleanup_bucket` takes the Bucket by value (deletes it). It still
  // resolves as a shared-object arg from its id.
  tx.moveCall({
    target: `${pkg}::bucket::cleanup_bucket`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.callCoinType],
    arguments: [
      tx.object(p.adminCapId),
      tx.object(p.bucketId),
      tx.object(SUI_CLOCK_OBJECT_ID),
    ],
  });
  return tx;
}

export type SetFeeBpsParams = {
  adminCapId: string;
  protocolConfigId: string;
  newBps: bigint;
};

export function buildSetFeeBpsTx(p: SetFeeBpsParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::admin::set_fee_bps`,
    arguments: [
      tx.object(p.adminCapId),
      tx.object(p.protocolConfigId),
      tx.pure.u64(p.newBps),
    ],
  });
  return tx;
}

export type WithdrawParams = {
  adminCapId: string;
  treasuryId: string;
  /** Move type `T` of the balance to withdraw, e.g. `0x…::tusdc::TUSDC`. */
  coinType: string;
  /** Amount in the coin's smallest units (raw u64). */
  amountRaw: bigint;
  recipient: string;
};

export function buildWithdrawTx(p: WithdrawParams): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::treasury::withdraw`,
    typeArguments: [p.coinType],
    arguments: [
      tx.object(p.adminCapId),
      tx.object(p.treasuryId),
      tx.pure.u64(p.amountRaw),
      tx.pure.address(p.recipient),
    ],
  });
  return tx;
}

export function buildCreateTreasuryTx(adminCapId: string): Transaction {
  const pkg = requirePackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::treasury::create_and_share`,
    arguments: [tx.object(adminCapId)],
  });
  return tx;
}

// ── Ingress whitelist + big red button (guarded launch) ────────────────
//
// Mirrors sui-tx admin.rs (IngressWhitelist / MarketPauseTarget): ONE
// standalone whitelist package with one shared `Whitelist` object and its
// own `whitelist::AdminCap`. Every mutation is a single call — there is no
// second list to keep in sync.

export type IngressWhitelistParams = {
  /** Owned `whitelist::AdminCap` (the standalone whitelist package's cap). */
  whitelistAdminCapId: string;
  /** The shared `Whitelist` object. */
  whitelistId: string;
};

function requireWhitelistPackage(): string {
  if (!WHITELIST_PACKAGE_ID) {
    throw new Error(
      `No whitelist deployment for VITE_ENVIRONMENT="${ENV}" — cannot build ingress whitelist PTBs`,
    );
  }
  return WHITELIST_PACKAGE_ID;
}

/** One `whitelist::<fn>(cap, wl, value)` call. */
function addWhitelistCall(
  tx: Transaction,
  p: IngressWhitelistParams,
  fn: "add_member" | "remove_member" | "set_whitelist_enabled" | "set_ingress_paused",
  value: TransactionArgument,
) {
  tx.moveCall({
    target: `${requireWhitelistPackage()}::whitelist::${fn}`,
    arguments: [tx.object(p.whitelistAdminCapId), tx.object(p.whitelistId), value],
  });
}

/** One PTB adding `member` to the ingress whitelist. */
export function buildWhitelistAddTx(p: IngressWhitelistParams, member: string): Transaction {
  const tx = new Transaction();
  addWhitelistCall(tx, p, "add_member", tx.pure.address(member));
  return tx;
}

/** One PTB removing `member` from the ingress whitelist. */
export function buildWhitelistRemoveTx(p: IngressWhitelistParams, member: string): Transaction {
  const tx = new Transaction();
  addWhitelistCall(tx, p, "remove_member", tx.pure.address(member));
  return tx;
}

/** One PTB flipping `set_whitelist_enabled` — the go-public lever.
 * Membership is retained on-chain, so re-enabling restores the prior
 * cohort. */
export function buildSetWhitelistEnabledTx(
  p: IngressWhitelistParams,
  enabled: boolean,
): Transaction {
  const tx = new Transaction();
  addWhitelistCall(tx, p, "set_whitelist_enabled", tx.pure.bool(enabled));
  return tx;
}

/** One exchange market's pause target (`config.EXCHANGE_MARKETS`). */
export type MarketPauseTarget = {
  registryId: string;
  base: string;
  quote: string;
};

export type IngressPauseParams = IngressWhitelistParams & {
  /** Core `AdminCap` — gates the trading-vault `registry::set_paused` leg. */
  coreAdminCapId: string;
  /** Shared `VaultProtocolConfig`; null when no trading-vault deployment
   * (its pause leg is omitted). */
  vaultProtocolConfigId: string | null;
  /** Exchange `exchange::admin::AdminCap`; null when not held / no exchange
   * deployment (the per-market legs are omitted). */
  exchangeAdminCapId: string | null;
  /** Every exchange market to `registry::set_paused<Base, Quote>`. */
  markets: MarketPauseTarget[];
};

/** The big red button, ONE PTB: `whitelist::set_ingress_paused` (whitelist
 * cap), trading-vault `registry::set_paused` (core cap), and exchange
 * `registry::set_paused<Base, Quote>` on every market (exchange cap). Legs
 * whose ids are absent are omitted. Exits (withdrawals/cancels) are never
 * gated on-chain, so flipping this strands nobody. */
function buildSetIngressPausedTx(p: IngressPauseParams, paused: boolean): Transaction {
  const tx = new Transaction();
  const flag = tx.pure.bool(paused);
  addWhitelistCall(tx, p, "set_ingress_paused", flag);
  if (p.vaultProtocolConfigId && TRADING_VAULT_PACKAGE_ID) {
    tx.moveCall({
      target: `${TRADING_VAULT_PACKAGE_ID}::registry::set_paused`,
      arguments: [tx.object(p.coreAdminCapId), tx.object(p.vaultProtocolConfigId), flag],
    });
  }
  if (p.exchangeAdminCapId && EXCHANGE_PACKAGE_ID) {
    for (const m of p.markets) {
      tx.moveCall({
        target: `${EXCHANGE_PACKAGE_ID}::registry::set_paused`,
        typeArguments: [m.base, m.quote],
        arguments: [tx.object(p.exchangeAdminCapId), tx.object(m.registryId), flag],
      });
    }
  }
  return tx;
}

export function buildPauseIngressTx(p: IngressPauseParams): Transaction {
  return buildSetIngressPausedTx(p, true);
}

export function buildUnpauseIngressTx(p: IngressPauseParams): Transaction {
  return buildSetIngressPausedTx(p, false);
}

// DEPRECATED (SO-332): the covered-call vault product is retired and the
// Admin screen no longer renders its pause/unpause controls. These builders
// are kept for reference and still target `options_vault`, so they throw
// unless the deployment predates the deprecation.
export type VaultPauseParams = {
  adminCapId: string;
  vaultId: string;
  /** The vault's `U` / `S` / `V` type args (from the api-service `Vault`). */
  underlyingCoinType: string;
  settlementCoinType: string;
  shareType: string;
};

function buildVaultPauseToggleTx(
  fn: "pause_deposits" | "unpause_deposits",
  p: VaultPauseParams,
): Transaction {
  const pkg = requireVaultPackage();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::vault::${fn}`,
    typeArguments: [p.underlyingCoinType, p.settlementCoinType, p.shareType],
    arguments: [tx.object(p.adminCapId), tx.object(p.vaultId)],
  });
  return tx;
}

/** Stop new deposits into a vault. Hard cutover: the backend then ignores it. */
export function buildPauseDepositsTx(p: VaultPauseParams): Transaction {
  return buildVaultPauseToggleTx("pause_deposits", p);
}

/** Re-open deposits on a previously paused vault. */
export function buildUnpauseDepositsTx(p: VaultPauseParams): Transaction {
  return buildVaultPauseToggleTx("unpause_deposits", p);
}
