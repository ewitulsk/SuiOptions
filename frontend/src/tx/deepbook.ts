// DeepBook v3 PTB builders (SO-154).
//
// Creating a trading venue for a bucket means calling
// `pool::create_permissionless_pool<CALL_i, TUSDC>` on DeepBook's UPGRADED
// package (calls execute there; only event/struct types resolve to the
// original publish). The contract hard-asserts the creation fee coin is
// exactly 500 DEEP — the REAL DeepBook testnet DEEP coin, not our TDEEP test
// token — so the connected wallet must hold it. The PTB shape mirrors the
// proven Rust harness (`rust-backend/tools/deepbook-pool-test`), and the
// gas-station template `deepbook_create_pool` pins exactly this shape.

import { Transaction, coinWithBalance } from "@mysten/sui/transactions";

import {
  DEEPBOOK_PACKAGE_ID,
  DEEPBOOK_POOL_CREATION_FEE,
  DEEPBOOK_REGISTRY_ID,
  DEEP_COIN_TYPE,
  ENV,
} from "../config";

/** True when token-info served a DeepBook deployment for this network. */
export function deepbookConfigured(): boolean {
  return Boolean(
    DEEPBOOK_PACKAGE_ID &&
      DEEPBOOK_REGISTRY_ID &&
      DEEP_COIN_TYPE &&
      DEEPBOOK_POOL_CREATION_FEE !== undefined,
  );
}

function requireDeepbook(): {
  pkg: string;
  registry: string;
  deepCoin: string;
  fee: bigint;
} {
  if (!deepbookConfigured()) {
    throw new Error(
      `No DeepBook deployment for VITE_ENVIRONMENT="${ENV}" (token-info returned no deepbook ids)`,
    );
  }
  return {
    pkg: DEEPBOOK_PACKAGE_ID!,
    registry: DEEPBOOK_REGISTRY_ID!,
    deepCoin: DEEP_COIN_TYPE!,
    fee: DEEPBOOK_POOL_CREATION_FEE!,
  };
}

export type VenueParams = {
  tickSize: bigint;
  lotSize: bigint;
  minSize: bigint;
};

/**
 * Derive DeepBook pool sizing from the pair's decimals.
 *
 * DeepBook prices are quote-per-base scaled by `10^(9 − baseDec + quoteDec)`
 * (verified against a live fill — DEEPBOOK-FINDINGS.md §C). All three values
 * are kept powers of 10 with `min_size` a multiple of `lot_size`, matching
 * the constraints the deployed contract accepts (the reference values for an
 * 8-dec base / 6-dec quote pair were validated by dry-run in
 * deepbook-pool-test).
 *
 * - `tickSize`: a $0.01 price step (scaled), floored at 1.
 * - `lotSize`: order-size step in base atomic units — 10^3 minimum (DeepBook
 *   floor), targeting ~10^-5 of a whole coin.
 * - `minSize`: smallest order, 10 lots.
 */
export function deriveVenueParams(baseDecimals: number, quoteDecimals: number): VenueParams {
  const priceExponent = 9 - baseDecimals + quoteDecimals;
  const tickSize = 10n ** BigInt(Math.max(0, priceExponent - 2));
  const lotSize = 10n ** BigInt(Math.max(3, baseDecimals - 5));
  const minSize = 10n * lotSize;
  return { tickSize, lotSize, minSize };
}

export type CreateVenueParams = {
  /** `bucket.call_coin_type` — the pool's base asset. */
  callCoinType: string;
  /** `series.settlement_coin_type` — the pool's quote asset (TUSDC). */
  settlementCoinType: string;
  venue: VenueParams;
};

/**
 * Build the create-venue PTB. `coinWithBalance` gathers exactly 500 DEEP
 * from the sender's coins (benign SplitCoins/MergeCoins prelude — the only
 * non-Move-call commands the gas-station template tolerates).
 */
export function buildCreateVenueTx(p: CreateVenueParams): Transaction {
  const { pkg, registry, deepCoin, fee } = requireDeepbook();
  const tx = new Transaction();
  tx.moveCall({
    target: `${pkg}::pool::create_permissionless_pool`,
    typeArguments: [p.callCoinType, p.settlementCoinType],
    arguments: [
      tx.object(registry), // &mut Registry (shared; dapp-kit resolves)
      tx.pure.u64(p.venue.tickSize),
      tx.pure.u64(p.venue.lotSize),
      tx.pure.u64(p.venue.minSize),
      coinWithBalance({ type: deepCoin, balance: fee }),
    ],
  });
  return tx;
}
