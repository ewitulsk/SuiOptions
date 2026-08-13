// PTB builders for the exchange-adapter curator surface (SO-373): custody
// creation, moving vault capital in/out of the shared exchange
// BalanceManager, and order-signer delegation.
//
// Shapes mirror the Move signatures in
// `contracts/exchange-adapter/sources/exchange_adapter.move`. All of these
// are curator session ops — NOT gas-sponsored, submit wallet-paid.

import { Transaction } from "@mysten/sui/transactions";

import {
  EXCHANGE_ADAPTER_PACKAGE_ID,
  EXCHANGE_WHITELIST_ID,
  TRADING_VAULT_OBJECTS,
} from "../config";

function requireRefs(): { pkg: string; integrationRegistryId: string } {
  if (!EXCHANGE_ADAPTER_PACKAGE_ID) {
    throw new Error("exchange-adapter package not deployed on this network");
  }
  if (!TRADING_VAULT_OBJECTS) {
    throw new Error("trading-vault governance objects not served by token-info");
  }
  return {
    pkg: EXCHANGE_ADAPTER_PACKAGE_ID,
    integrationRegistryId: TRADING_VAULT_OBJECTS.integrationRegistryId,
  };
}

export type InitExchangeCustodyParams = {
  vaultId: string;
  /** The curator's owned `CuratorCap` object id. */
  curatorCapId: string;
};

/**
 * `exchange_adapter::init_custody` — create the vault's cap-owned exchange
 * `BalanceManager` (funded mode) and custody its OwnerCap as a vault
 * position. The custody + manager ids are read back from the tx effects.
 */
export function buildInitExchangeCustodyTx(p: InitExchangeCustodyParams): Transaction {
  const { pkg, integrationRegistryId } = requireRefs();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::exchange_adapter::init_custody`,
    arguments: [
      tx.object(p.vaultId),
      tx.object(p.curatorCapId),
      tx.object(integrationRegistryId),
    ],
  });
  return tx;
}

export type ExchangeCustodyMoveParams = {
  vaultId: string;
  /** The curator's owned `CuratorCap` object id. */
  curatorCapId: string;
  /** The custody position id (an `ID` pure arg, not an object input). */
  custodyId: string;
  /** The custody's SHARED exchange `BalanceManager` object id. */
  bmId: string;
  /** Asset moved — the `T` type arg. */
  coinType: string;
  /** Amount in smallest units. */
  amountRaw: bigint;
};

/** `exchange_adapter::fund<T>` — move vault free balance into the exchange
 * BalanceManager (auto-tracked for appraisals). Funded custodies only. */
export function buildExchangeFundTx(p: ExchangeCustodyMoveParams): Transaction {
  return custodyMoveTx("fund", p);
}

/** `exchange_adapter::defund<T>` — move BalanceManager funds back into the
 * vault's free balances (auto-untracked once the manager's `T` hits zero). */
export function buildExchangeDefundTx(p: ExchangeCustodyMoveParams): Transaction {
  return custodyMoveTx("defund", p);
}

function custodyMoveTx(fn: "fund" | "defund", p: ExchangeCustodyMoveParams): Transaction {
  const { pkg, integrationRegistryId } = requireRefs();
  const tx = new Transaction();
  // `fund` deposits into the exchange BalanceManager, which is
  // ingress-gated by the shared exchange Whitelist (SO-384). `defund`
  // moves value OUT and stays ungated.
  const wl: string[] = [];
  if (fn === "fund") {
    if (!EXCHANGE_WHITELIST_ID) {
      throw new Error(
        "exchange record has no whitelistId — cannot fund an exchange custody (SO-384)",
      );
    }
    wl.push(EXCHANGE_WHITELIST_ID);
  }
  tx.moveCall({
    target: `${pkg}::exchange_adapter::${fn}`,
    typeArguments: [p.coinType],
    arguments: [
      tx.object(p.vaultId),
      tx.object(p.curatorCapId),
      tx.object(integrationRegistryId),
      ...wl.map((id) => tx.object(id)),
      tx.object(p.bmId),
      tx.pure.id(p.custodyId),
      tx.pure.u64(p.amountRaw),
    ],
  });
  return tx;
}

export type ExchangeSignerParams = {
  vaultId: string;
  /** The curator's owned `CuratorCap` object id. */
  curatorCapId: string;
  custodyId: string;
  bmId: string;
  /** The delegated order-signing hot key (the curator's maker bot). */
  signer: string;
};

/** `exchange_adapter::add_signer` — delegate an order-signing hot key. */
export function buildExchangeAddSignerTx(p: ExchangeSignerParams): Transaction {
  return signerTx("add_signer", p);
}

/** `exchange_adapter::remove_signer` — instantly voids that key's
 * outstanding orders. */
export function buildExchangeRemoveSignerTx(p: ExchangeSignerParams): Transaction {
  return signerTx("remove_signer", p);
}

function signerTx(fn: "add_signer" | "remove_signer", p: ExchangeSignerParams): Transaction {
  const { pkg, integrationRegistryId } = requireRefs();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::exchange_adapter::${fn}`,
    arguments: [
      tx.object(p.vaultId),
      tx.object(p.curatorCapId),
      tx.object(integrationRegistryId),
      tx.object(p.bmId),
      tx.pure.id(p.custodyId),
      tx.pure.address(p.signer),
    ],
  });
  return tx;
}
